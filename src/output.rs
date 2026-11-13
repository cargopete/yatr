//! Terminal output that survives a closed pipe.
//!
//! `println!` panics when stdout is gone (`yatr list | head -1`). These write
//! the same way but drop output once a stream reports a broken pipe, so a
//! closed reader never aborts a run that is still doing real work.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static STDOUT_CLOSED: AtomicBool = AtomicBool::new(false);
static STDERR_CLOSED: AtomicBool = AtomicBool::new(false);

pub fn stdout(args: std::fmt::Arguments) {
    write(&STDOUT_CLOSED, std::io::stdout().lock(), args);
}

pub fn stderr(args: std::fmt::Arguments) {
    write(&STDERR_CLOSED, std::io::stderr().lock(), args);
}

fn write(closed: &AtomicBool, mut stream: impl Write, args: std::fmt::Arguments) {
    if closed.load(Ordering::Relaxed) {
        return;
    }
    let result = stream.write_fmt(args).and_then(|()| stream.flush());
    if matches!(result, Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe) {
        closed.store(true, Ordering::Relaxed);
    }
}

macro_rules! out {
    ($($arg:tt)*) => {
        $crate::output::stdout(format_args!($($arg)*))
    };
}

macro_rules! outln {
    () => {
        $crate::output::stdout(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::output::stdout(format_args!("{}\n", format_args!($($arg)*)))
    };
}

macro_rules! errln {
    () => {
        $crate::output::stderr(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::output::stderr(format_args!("{}\n", format_args!($($arg)*)))
    };
}

pub(crate) use {errln, out, outln};
