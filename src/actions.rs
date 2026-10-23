use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};

pub struct DeleteTask(Receiver<Result<(), String>>);

impl DeleteTask {
    pub fn start(path: PathBuf) -> Result<Self, String> {
        let (tx, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("windirstat-delete".into())
            .spawn(move || {
                let result = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                let _ = tx.send(result.map_err(|error| error.to_string()));
            })
            .map_err(|error| error.to_string())?;
        Ok(Self(receiver))
    }

    pub fn poll(&self) -> Option<Result<(), String>> {
        match self.0.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Err("Поток удаления завершился без результата".to_owned()))
            }
        }
    }
}

#[cfg(windows)]
pub fn open(path: &std::path::Path, is_directory: bool) -> Result<(), String> {
    let mut command = std::process::Command::new("explorer.exe");
    if is_directory {
        command.arg(path);
    } else {
        let mut selection = std::ffi::OsString::from("/select,");
        selection.push(path);
        command.arg(selection);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
pub fn relaunch_elevated(root: &std::path::Path) -> Result<(), String> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_command: i32,
        ) -> *mut c_void;
    }
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let exe_wide: Vec<_> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let operation: Vec<_> = "runas".encode_utf16().chain(Some(0)).collect();
    let mut parameters: Vec<_> = "--scan ".encode_utf16().collect();
    parameters.extend(quote_argument(root.as_os_str()));
    parameters.push(0);
    // All arguments point to NUL-terminated UTF-16 buffers alive for the call.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            exe_wide.as_ptr(),
            parameters.as_ptr(),
            std::ptr::null(),
            1,
        )
    } as isize;
    if result <= 32 {
        Err(format!(
            "Не удалось перезапустить от администратора (код {result})"
        ))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn quote_argument(argument: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut quoted = vec![u16::from(b'"')];
    let mut backslashes = 0;
    for unit in argument.encode_wide() {
        if unit == u16::from(b'\\') {
            backslashes += 1;
            continue;
        }
        let count = if unit == u16::from(b'"') {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        quoted.extend(std::iter::repeat_n(u16::from(b'\\'), count));
        quoted.push(unit);
        backslashes = 0;
    }
    quoted.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    quoted.push(u16::from(b'"'));
    quoted
}
