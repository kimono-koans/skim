/// helper for turn a BufRead into a skim stream
use std::env;
use std::error::Error;
use std::io::{BufRead, BufReader};

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use regex::Regex;

use crate::field::FieldRange;
use crate::helper::ingest::{ingest_loop, BuildOptions, SendRawOrBuild};
use crate::reader::CommandCollector;
use crate::{SkimItem, SkimItemReceiver, SkimItemSender};

#[cfg(feature = "malloc_trim")]
#[cfg(target_os = "linux")]
#[cfg(target_env = "gnu")]
use crate::malloc_trim;

const CMD_CHANNEL_SIZE: usize = 1_024;
const DELIMITER_STR: &str = r"[\t\n ]+";

pub enum CollectorInput {
    Pipe(Box<dyn BufRead + Send>),
    Command(String),
}

#[derive(Debug)]
pub struct SkimItemReaderOption {
    use_ansi_color: bool,
    transform_fields: Vec<FieldRange>,
    matching_fields: Vec<FieldRange>,
    delimiter: Regex,
    line_ending: u8,
    show_error: bool,
}

impl Default for SkimItemReaderOption {
    fn default() -> Self {
        Self {
            line_ending: b'\n',
            use_ansi_color: false,
            transform_fields: Vec::new(),
            matching_fields: Vec::new(),
            delimiter: Regex::new(DELIMITER_STR).unwrap(),
            show_error: false,
        }
    }
}

impl SkimItemReaderOption {
    pub fn line_ending(mut self, line_ending: u8) -> Self {
        self.line_ending = line_ending;
        self
    }

    pub fn ansi(mut self, enable: bool) -> Self {
        self.use_ansi_color = enable;
        self
    }

    pub fn delimiter(mut self, delimiter: &str) -> Self {
        if !delimiter.is_empty() {
            self.delimiter = Regex::new(delimiter).unwrap_or_else(|_| Regex::new(DELIMITER_STR).unwrap());
        }
        self
    }

    pub fn with_nth(mut self, with_nth: &str) -> Self {
        if !with_nth.is_empty() {
            self.transform_fields = with_nth.split(',').filter_map(FieldRange::from_str).collect();
        }
        self
    }

    pub fn transform_fields(mut self, transform_fields: Vec<FieldRange>) -> Self {
        self.transform_fields = transform_fields;
        self
    }

    pub fn nth(mut self, nth: &str) -> Self {
        if !nth.is_empty() {
            self.matching_fields = nth.split(',').filter_map(FieldRange::from_str).collect();
        }
        self
    }

    pub fn matching_fields(mut self, matching_fields: Vec<FieldRange>) -> Self {
        self.matching_fields = matching_fields;
        self
    }

    pub fn read0(mut self, enable: bool) -> Self {
        if enable {
            self.line_ending = b'\0';
        } else {
            self.line_ending = b'\n';
        }
        self
    }

    pub fn show_error(mut self, show_error: bool) -> Self {
        self.show_error = show_error;
        self
    }

    pub fn build(self) -> Self {
        self
    }

    pub fn is_simple(&self) -> bool {
        !self.use_ansi_color && self.matching_fields.is_empty() && self.transform_fields.is_empty()
    }
}

pub struct SkimItemReader {
    option: Arc<SkimItemReaderOption>,
}

impl Default for SkimItemReader {
    fn default() -> Self {
        Self {
            option: Arc::new(Default::default()),
        }
    }
}

impl SkimItemReader {
    pub fn new(option: SkimItemReaderOption) -> Self {
        Self {
            option: Arc::new(option),
        }
    }

    pub fn option(mut self, option: SkimItemReaderOption) -> Self {
        self.option = Arc::new(option);
        self
    }
}

impl SkimItemReader {
    pub fn of_bufread(&self, source: Box<dyn BufRead + Send>) -> (SkimItemReceiver, Option<JoinHandle<()>>) {
        if self.option.is_simple() {
            self.raw_bufread(source)
        } else {
            let (rx_item, _tx_item, opt_ingest_handle) = self
                .read_and_collect_from_command(Arc::new(AtomicUsize::new(0)), CollectorInput::Pipe(Box::new(source)));
            (rx_item, opt_ingest_handle)
        }
    }

    /// helper: convert bufread into SkimItemReceiver
    fn raw_bufread(&self, source: Box<dyn BufRead + Send>) -> (SkimItemReceiver, Option<JoinHandle<()>>) {
        let (tx_item, rx_item): (SkimItemSender, SkimItemReceiver) = unbounded();
        let line_ending = self.option.line_ending;

        let ingest_handle = thread::spawn(move || {
            ingest_loop(source, line_ending, tx_item, SendRawOrBuild::Raw);

            #[cfg(feature = "malloc_trim")]
            #[cfg(target_os = "linux")]
            #[cfg(target_env = "gnu")]
            malloc_trim();
        });

        (rx_item, Some(ingest_handle))
    }

    /// components_to_stop == 0 => all the threads have been stopped
    /// return (channel_for_receive_item, channel_to_stop_command)
    #[allow(clippy::type_complexity)]
    fn read_and_collect_from_command(
        &self,
        components_to_stop: Arc<AtomicUsize>,
        input: CollectorInput,
    ) -> (Receiver<Arc<dyn SkimItem>>, Sender<i32>, Option<JoinHandle<()>>) {
        let (tx_interrupt, rx_interrupt) = bounded(CMD_CHANNEL_SIZE);
        let (tx_item, rx_item): (SkimItemSender, SkimItemReceiver) = unbounded();

        match input {
            CollectorInput::Pipe(source) => {
                let started = Arc::new(AtomicBool::new(false));
                let started_clone = started.clone();
                let tx_interrupt_clone = tx_interrupt.clone();
                let option = self.option.clone();
                let ingest_handle = thread::spawn(move || {
                    debug!("collector: command collector start");
                    components_to_stop.fetch_add(1, Ordering::SeqCst);
                    started_clone.store(true, Ordering::SeqCst);
                    // notify parent that it is started

                    let opts = BuildOptions {
                        ansi_enabled: option.use_ansi_color,
                        trans_fields: &option.transform_fields,
                        matching_fields: &option.matching_fields,
                        delimiter: &option.delimiter,
                    };

                    ingest_loop(source, option.line_ending, tx_item, SendRawOrBuild::Build(opts));

                    let _ = tx_interrupt_clone.send(1); // ensure the waiting thread will exit
                    components_to_stop.fetch_sub(1, Ordering::SeqCst);
                    debug!("collector: command collector stop");
                });

                while !started.load(Ordering::SeqCst) {
                    // busy waiting for the thread to start. (components_to_stop is added)
                }

                (rx_item, tx_interrupt, Some(ingest_handle))
            }
            CollectorInput::Command(cmd) => {
                let command = get_command_output(&cmd).expect("command not found").0;

                let started = Arc::new(AtomicBool::new(false));
                let started_clone = started.clone();
                let components_to_stop_clone = components_to_stop;
                let send_error = self.option.show_error;
                // listening to close signal and kill command if needed
                let ingest_handle = thread::spawn(move || {
                    debug!("collector: command killer start");
                    components_to_stop_clone.fetch_add(1, Ordering::SeqCst);
                    started_clone.store(true, Ordering::SeqCst); // notify parent that it is started

                    let _ = rx_interrupt.recv(); // block waiting
                    if let Some(mut child) = command {
                        // clean up resources
                        let _ = child.kill();
                        let _ = child.wait();

                        if send_error {
                            let has_error = child
                                .try_wait()
                                .map(|os| os.map(|s| !s.success()).unwrap_or(true))
                                .unwrap_or(false);
                            if has_error {
                                let output = child.wait_with_output().expect("could not retrieve error message");
                                for line in String::from_utf8_lossy(&output.stderr).lines() {
                                    let _ = tx_item.send(Arc::new(line.to_string()));
                                }
                            }
                        }
                    }

                    components_to_stop_clone.fetch_sub(1, Ordering::SeqCst);
                    debug!("collector: command killer stop");
                });

                while !started.load(Ordering::SeqCst) {
                    // busy waiting for the thread to start. (components_to_stop is added)
                }

                (rx_item, tx_interrupt, Some(ingest_handle))
            }
        }
    }
}

impl CommandCollector for SkimItemReader {
    fn invoke(
        &mut self,
        cmd: &str,
        components_to_stop: Arc<AtomicUsize>,
    ) -> (SkimItemReceiver, Sender<i32>, Option<JoinHandle<()>>) {
        self.read_and_collect_from_command(components_to_stop, CollectorInput::Command(cmd.to_string()))
    }
}

type CommandOutput = (Option<Child>, Box<dyn BufRead + Send>);

fn get_command_output(cmd: &str) -> Result<CommandOutput, Box<dyn Error>> {
    let shell = env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    let mut command: Child = Command::new(shell)
        .arg("-c")
        .arg(cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = command
        .stdout
        .take()
        .ok_or_else(|| "command output: unwrap failed".to_owned())?;

    Ok((Some(command), Box::new(BufReader::new(stdout))))
}
