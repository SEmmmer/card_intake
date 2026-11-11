//! One cancellable ExifTool process, with UTF-8 arguments and small responses.
use std::{
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};

pub(super) struct Session {
    child: Child,
    input: ChildStdin,
    output: mpsc::Receiver<io::Result<Vec<u8>>>,
    reader: Option<JoinHandle<()>>,
}
impl Session {
    pub fn start(tool: &Path) -> io::Result<Self> {
        let mut child = crate::process::command(tool)
            .args(["-stay_open", "True", "-@", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, output) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut stream = io::BufReader::new(stdout);
            loop {
                let mut bytes = Vec::new();
                loop {
                    let mut line = Vec::new();
                    match stream.read_until(b'\n', &mut line) {
                        Ok(0) => {
                            let _ = tx.send(Err(io::Error::other(
                                "ExifTool ended before returning settings",
                            )));
                            return;
                        }
                        Ok(_) if line.starts_with(b"{ready}") => break,
                        Ok(_) => bytes.extend(line),
                        Err(error) => {
                            let _ = tx.send(Err(error));
                            return;
                        }
                    }
                }
                if tx.send(Ok(bytes)).is_err() {
                    return;
                }
            }
        });
        Ok(Self {
            child,
            input,
            output,
            reader: Some(reader),
        })
    }
    pub fn read(
        &mut self,
        photos: &[PathBuf],
        tags: &[&str],
        cancel: &AtomicBool,
    ) -> io::Result<Vec<u8>> {
        let mut args = String::from("-json\n-n\n-fast\n-charset\nfilename=UTF8\n");
        for tag in tags {
            args.push_str(tag);
            args.push('\n');
        }
        for photo in photos {
            let name = photo.to_string_lossy();
            if name.contains(['\r', '\n']) {
                return Err(io::Error::other("Invalid raw filename"));
            }
            args.push_str(&name);
            args.push('\n');
        }
        args.push_str("-execute\n");
        self.input.write_all(args.as_bytes())?;
        self.input.flush()?;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::Error::other("Rating read stopped"));
            }
            match self.output.recv_timeout(Duration::from_millis(20)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => return Err(io::Error::other("ExifTool stopped")),
            }
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
