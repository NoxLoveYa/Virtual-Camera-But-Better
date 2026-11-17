//! Small helpers shared by the subcommands.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Output sink of the elevated child: stdout (hidden console) plus a result file the parent prints.
pub struct Out {
    file: Option<PathBuf>,
}

impl Out {
    pub fn new(file: Option<&Path>) -> Out {
        if let Some(f) = file {
            let _ = std::fs::write(f, b"");
        }
        Out {
            file: file.map(|p| p.to_path_buf()),
        }
    }

    pub fn line(&mut self, s: &str) {
        println!("{s}");
        if let Some(f) = &self.file {
            if let Ok(mut h) = OpenOptions::new().create(true).append(true).open(f) {
                let _ = writeln!(h, "{s}");
            }
        }
    }
}

impl vcam_install::Log for Out {
    fn line(&mut self, s: &str) {
        Out::line(self, s);
    }
}
