use super::output::OutputState;
use super::state::ProcessError;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, mpsc, oneshot};

const UNIFIED_EXEC_ENV: [(&str, &str); 10] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("LANG", "C.UTF-8"),
    ("LC_CTYPE", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
    ("COLORTERM", ""),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GH_PAGER", "cat"),
    ("CODEX_CI", "1"),
];

enum Control {
    Write(Vec<u8>, oneshot::Sender<Result<(), std::io::Error>>),
    Interrupt(oneshot::Sender<Result<(), std::io::Error>>),
    Terminate(oneshot::Sender<Result<(), std::io::Error>>),
}

enum PtyWriterControl {
    Write(Vec<u8>, oneshot::Sender<Result<(), std::io::Error>>),
    Interrupt(oneshot::Sender<Result<(), std::io::Error>>),
}

pub(crate) struct RunningProcess {
    pub(crate) output: Arc<OutputState>,
    pub(crate) interaction: Arc<Mutex<()>>,
    pub(crate) tty: bool,
    process_id: Option<u32>,
    control: mpsc::UnboundedSender<Control>,
}

impl RunningProcess {
    pub(crate) async fn write(&self, bytes: Vec<u8>) -> Result<(), ProcessError> {
        let (tx, rx) = oneshot::channel();
        self.control
            .send(Control::Write(bytes, tx))
            .map_err(|_| ProcessError::Interaction(std::io::ErrorKind::BrokenPipe.into()))?;
        rx.await
            .map_err(|_| ProcessError::Interaction(std::io::ErrorKind::BrokenPipe.into()))?
            .map_err(ProcessError::Interaction)
    }

    pub(crate) async fn interrupt(&self) -> Result<(), ProcessError> {
        let (tx, rx) = oneshot::channel();
        self.control
            .send(Control::Interrupt(tx))
            .map_err(|_| ProcessError::Interaction(std::io::ErrorKind::BrokenPipe.into()))?;
        rx.await
            .map_err(|_| ProcessError::Interaction(std::io::ErrorKind::BrokenPipe.into()))?
            .map_err(ProcessError::Interaction)
    }

    pub(crate) async fn terminate(&self) {
        if terminate_process_now(self.process_id).is_ok() {
            return;
        }
        let (tx, rx) = oneshot::channel();
        if self.control.send(Control::Terminate(tx)).is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(1), rx).await;
        }
    }

    pub(crate) fn terminate_detached(&self) {
        if terminate_process_now(self.process_id).is_ok() {
            return;
        }
        let (tx, _rx) = oneshot::channel();
        let _ = self.control.send(Control::Terminate(tx));
    }
}

pub(crate) fn spawn(
    mut command: std::process::Command,
    tty: bool,
) -> Result<Arc<RunningProcess>, ProcessError> {
    command.envs(UNIFIED_EXEC_ENV);
    if tty {
        spawn_pty(&command)
    } else {
        spawn_pipe(command)
    }
}

fn spawn_pipe(mut command: std::process::Command) -> Result<Arc<RunningProcess>, ProcessError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = tokio::process::Command::from(command)
        .spawn()
        .map_err(ProcessError::spawn)?;
    let process_id = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProcessError::spawn(std::io::Error::other("missing child stdout")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProcessError::spawn(std::io::Error::other("missing child stderr")))?;
    let output = Arc::new(OutputState::new(2));
    spawn_async_reader(stdout, Arc::clone(&output));
    spawn_async_reader(stderr, Arc::clone(&output));

    let (control, mut rx) = mpsc::unbounded_channel();
    let actor_output = Arc::clone(&output);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(20));
        let mut forced_exit_code = None;
        let mut control_closed = false;
        let exit_code = loop {
            tokio::select! {
                _ = interval.tick() => {
                    match child.try_wait() {
                        Ok(Some(status)) => break status.code().or(forced_exit_code),
                        Ok(None) => {}
                        Err(_) => break None,
                    }
                }
                control = rx.recv(), if !control_closed => match control {
                    Some(Control::Write(_, response)) => {
                        let _ = response.send(Err(std::io::ErrorKind::BrokenPipe.into()));
                    }
                    Some(Control::Interrupt(response)) => {
                        let result = signal_process_tree(process_id, "INT")
                            .or_else(|_| child.start_kill());
                        if result.is_ok() {
                            forced_exit_code = Some(130);
                        }
                        let _ = response.send(result);
                    }
                    Some(Control::Terminate(response)) => {
                        let result = signal_process_tree(process_id, "KILL")
                            .or_else(|_| child.start_kill());
                        if result.is_ok() {
                            forced_exit_code = Some(137);
                        }
                        let _ = response.send(result);
                    }
                    None => {
                        let _ = signal_process_tree(process_id, "KILL")
                            .or_else(|_| child.start_kill());
                        forced_exit_code = Some(137);
                        control_closed = true;
                    }
                }
            }
        };
        actor_output.mark_exit(exit_code);
        while !actor_output.is_closed() {
            let Ok(Some(control)) =
                tokio::time::timeout(Duration::from_millis(20), rx.recv()).await
            else {
                continue;
            };
            match control {
                Control::Write(_, response) => {
                    let _ = response.send(Err(std::io::ErrorKind::BrokenPipe.into()));
                }
                Control::Interrupt(response) => {
                    let _ = response.send(signal_process_tree(process_id, "INT"));
                }
                Control::Terminate(response) => {
                    let _ = response.send(terminate_process_now(process_id));
                }
            }
        }
    });

    Ok(Arc::new(RunningProcess {
        output,
        interaction: Arc::new(Mutex::new(())),
        tty: false,
        process_id,
        control,
    }))
}

fn spawn_async_reader<R>(mut reader: R, output: Arc<OutputState>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut bytes = vec![0_u8; 8 * 1024];
        loop {
            match reader.read(&mut bytes).await {
                Ok(0) | Err(_) => break,
                Ok(count) => output.push(bytes[..count].to_vec()),
            }
        }
        output.reader_closed();
    });
}

fn spawn_pty_writer(
    mut writer: Box<dyn Write + Send>,
) -> Result<mpsc::UnboundedSender<PtyWriterControl>, ProcessError> {
    let (writer_control, mut writer_rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("mcp-agent-pty-writer".to_owned())
        .spawn(move || {
            while let Some(control) = writer_rx.blocking_recv() {
                match control {
                    PtyWriterControl::Write(bytes, response) => {
                        let result = writer.write_all(&bytes).and_then(|()| writer.flush());
                        let _ = response.send(result);
                    }
                    PtyWriterControl::Interrupt(response) => {
                        let result = writer.write_all(b"\x03").and_then(|()| writer.flush());
                        let _ = response.send(result);
                    }
                }
            }
        })
        .map_err(ProcessError::spawn)?;
    Ok(writer_control)
}

fn reject_pty_writer_control(control: PtyWriterControl) {
    let response = match control {
        PtyWriterControl::Write(_, response) | PtyWriterControl::Interrupt(response) => response,
    };
    let _ = response.send(Err(std::io::ErrorKind::BrokenPipe.into()));
}

fn serve_pty_descendant_controls(
    rx: &mut mpsc::UnboundedReceiver<Control>,
    output: &OutputState,
    process_id: Option<u32>,
) {
    while !output.is_closed() {
        while let Ok(control) = rx.try_recv() {
            match control {
                Control::Write(_, response) => {
                    let _ = response.send(Err(std::io::ErrorKind::BrokenPipe.into()));
                }
                Control::Interrupt(response) => {
                    let _ = response.send(signal_process_tree(process_id, "INT"));
                }
                Control::Terminate(response) => {
                    let _ = response.send(terminate_process_now(process_id));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn spawn_pty(command: &std::process::Command) -> Result<Arc<RunningProcess>, ProcessError> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| ProcessError::spawn(std::io::Error::other(error.to_string())))?;
    let mut builder = CommandBuilder::new(command.get_program());
    builder.args(command.get_args());
    if let Some(cwd) = command.get_current_dir() {
        builder.cwd(cwd);
    }
    for (key, value) in command.get_envs() {
        match value {
            Some(value) => builder.env(key, value),
            None => builder.env_remove(key),
        }
    }
    let mut child = pair
        .slave
        .spawn_command(builder)
        .map_err(|error| ProcessError::spawn(std::io::Error::other(error.to_string())))?;
    let process_id = child.process_id();
    drop(pair.slave);
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| ProcessError::spawn(std::io::Error::other(error.to_string())))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| ProcessError::spawn(std::io::Error::other(error.to_string())))?;
    let output = Arc::new(OutputState::new(1));
    let reader_output = Arc::clone(&output);
    std::thread::Builder::new()
        .name("mcp-agent-pty-reader".to_owned())
        .spawn(move || {
            let mut bytes = vec![0_u8; 8 * 1024];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => reader_output.push(bytes[..count].to_vec()),
                }
            }
            reader_output.reader_closed();
        })
        .map_err(ProcessError::spawn)?;

    let writer_control = spawn_pty_writer(writer)?;

    let (control, mut rx) = mpsc::unbounded_channel();
    let actor_output = Arc::clone(&output);
    std::thread::Builder::new()
        .name("mcp-agent-pty-process".to_owned())
        .spawn(move || {
            let exit_code = loop {
                while let Ok(control) = rx.try_recv() {
                    match control {
                        Control::Write(bytes, response) => {
                            if let Err(error) =
                                writer_control.send(PtyWriterControl::Write(bytes, response))
                            {
                                reject_pty_writer_control(error.0);
                            }
                        }
                        Control::Interrupt(response) => {
                            if signal_process_tree(process_id, "INT").is_ok() {
                                let _ = response.send(Ok(()));
                            } else if let Err(error) =
                                writer_control.send(PtyWriterControl::Interrupt(response))
                            {
                                reject_pty_writer_control(error.0);
                            }
                        }
                        Control::Terminate(response) => {
                            let result =
                                terminate_process_now(process_id).or_else(|_| child.kill());
                            let _ = response.send(result);
                        }
                    }
                }
                match child.try_wait() {
                    Ok(Some(status)) => {
                        break i32::try_from(status.exit_code()).ok();
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                    Err(_) => break None,
                }
            };
            actor_output.mark_exit(exit_code);
            serve_pty_descendant_controls(&mut rx, &actor_output, process_id);
        })
        .map_err(ProcessError::spawn)?;

    Ok(Arc::new(RunningProcess {
        output,
        interaction: Arc::new(Mutex::new(())),
        tty: true,
        process_id,
        control,
    }))
}

#[cfg(unix)]
fn signal_process_tree(process_id: Option<u32>, signal: &str) -> std::io::Result<()> {
    let process_id = process_id.ok_or_else(|| std::io::Error::other("missing process id"))?;
    let status = std::process::Command::new("/bin/kill")
        .arg(format!("-{signal}"))
        .arg(format!("-{process_id}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("process-group signal failed"))
    }
}

fn terminate_process_now(process_id: Option<u32>) -> std::io::Result<()> {
    signal_process_tree(process_id, "KILL").or_else(|_| signal_process(process_id, "KILL"))
}

#[cfg(unix)]
fn signal_process(process_id: Option<u32>, signal: &str) -> std::io::Result<()> {
    let process_id = process_id.ok_or_else(|| std::io::Error::other("missing process id"))?;
    let status = std::process::Command::new("/bin/kill")
        .arg(format!("-{signal}"))
        .arg(process_id.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("process signal failed"))
    }
}

#[cfg(not(unix))]
fn signal_process(_process_id: Option<u32>, _signal: &str) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "native process signaling is handled by the sandbox helper",
    ))
}

#[cfg(not(unix))]
fn signal_process_tree(_process_id: Option<u32>, _signal: &str) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "native process-tree signaling is handled by the sandbox helper",
    ))
}
