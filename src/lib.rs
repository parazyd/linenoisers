//! linenoise -- Guerilla line editing library against the idea that
//! a line editing lib needs to be 20,000 lines of C code.
//!
//! Does a number of crazy assumptions that happen to be true in
//! 99.9999% of the UNIX computers around these days.
//!
//! Copyright (c) 2025 parazyd <parazyd at dyne dot org>
//! Copyright (c) 2010-2023, Salvatore Sanfilippo <antirez at gmail dot com>
//! Copyright (c) 2010-2013, Pieter Noordhuis <pcnoordhuis at gmail dot com>
//!
//! All rights reserved.
//!
//! Redistribution and use in source and binary forms, with or without
//! modification, are permitted provided that the following conditions are
//! met:
//!
//! *  Redistributions of source code must retain the above copyright
//!    notice, this list of conditions and the following disclaimer.
//!
//! *  Redistributions in binary form must reproduce the above copyright
//!    notice, this list of conditions and the following disclaimer in the
//!    documentation and/or other materials provided with the distribution.
//!
//! THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
//! "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
//! LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
//! A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
//! HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
//! SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
//! LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
//! DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
//! THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
//! (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
//! OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//!
//! References:
//! - http://invisible-island.net/xterm/ctlseqs/ctlseqs.html
//! - http://www.3waylabs.com/nw/WWW/products/wizcon/vt220.html

#![allow(clippy::manual_div_ceil)]
#![allow(clippy::manual_range_contains)]

use std::cmp::min;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::io::RawFd;
use std::sync::Mutex;
use std::{env, mem};

use libc::{c_void, tcgetattr, tcsetattr, termios};

// Constants
const LINENOISE_DEFAULT_HISTORY_MAX_LEN: usize = 100;
const LINENOISE_MAX_LINE: usize = 4096;

// Key codes
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Key {
    CtrlA = 1,
    CtrlB = 2,
    CtrlC = 3,
    CtrlD = 4,
    CtrlE = 5,
    CtrlF = 6,
    CtrlH = 8,
    Tab = 9,
    CtrlK = 11,
    CtrlL = 12,
    Enter = 13,
    CtrlN = 14,
    CtrlP = 16,
    CtrlT = 20,
    CtrlU = 21,
    CtrlW = 23,
    Esc = 27,
    Backspace = 127,
}

// Callback types
pub type CompletionCallback = fn(&str, &mut Vec<String>);
pub type HintsCallback = fn(&str) -> Option<(String, i32, bool)>;

lazy_static::lazy_static! {
    static ref G: Mutex<GlobalState> = Mutex::new(GlobalState::new());
}

struct GlobalState {
    /// Multi-line mode. Default is single line.
    multi_line: bool,
    /// Show "***" instead of input. For passwords.
    mask_mode: bool,
    /// Input history.
    history: History,
    /// Callback for showing input completion.
    completion_callback: Option<CompletionCallback>,
    /// Callback for showing input hints.
    hints_callback: Option<HintsCallback>,
    /// For `atexit()` to check if restore is needed.
    raw_mode: bool,
    /// In order to restore at exit.
    orig_termios: Option<termios>,
}

impl GlobalState {
    fn new() -> Self {
        GlobalState {
            multi_line: false,
            mask_mode: false,
            history: History::new(),
            completion_callback: None,
            hints_callback: None,
            raw_mode: false,
            orig_termios: None,
        }
    }
}

// History management
#[derive(Clone)]
struct History {
    max_len: usize,
    entries: VecDeque<String>,
}

impl History {
    fn new() -> Self {
        History {
            max_len: LINENOISE_DEFAULT_HISTORY_MAX_LEN,
            entries: VecDeque::new(),
        }
    }

    fn add(&mut self, line: &str) -> bool {
        if self.max_len == 0 || line.is_empty() {
            return false;
        }

        // Don't add duplicates
        if self.entries.back().is_some_and(|last| last == line) {
            return false;
        }

        // Trim to max length
        if self.entries.len() >= self.max_len {
            self.entries.pop_front();
        }

        self.entries.push_back(line.to_string());
        true
    }

    fn get(&self, index: usize) -> Option<&str> {
        self.entries
            .get(self.entries.len().wrapping_sub(index))
            .map(|s| s.as_str())
    }
}

// Terminal handling
struct Terminal {
    ifd: RawFd,
    ofd: RawFd,
    cols: usize,
}

/// RAII guard that restores terminal to original mode when dropped
struct RawModeGuard {
    ifd: RawFd,
    orig_termios: termios,
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        unsafe {
            tcsetattr(self.ifd, libc::TCSAFLUSH, &self.orig_termios);
        }
        // Also update global state
        if let Ok(mut state) = G.lock() {
            state.raw_mode = false;
            state.orig_termios = None;
        }
    }
}

impl Terminal {
    fn new(ifd: RawFd, ofd: RawFd) -> Self {
        let cols = Self::get_columns(ifd, ofd);
        Terminal { ifd, ofd, cols }
    }

    /// Raw mode: 1960 magic shit.
    fn enable_raw_mode(&self) -> io::Result<RawModeGuard> {
        if !self.is_tty() {
            return Err(io::Error::other("Not a TTY"));
        }

        let mut orig = unsafe { mem::zeroed::<termios>() };
        if unsafe { tcgetattr(self.ifd, &mut orig) } == -1 {
            return Err(io::Error::last_os_error());
        }

        // Modify the original mode
        let mut raw = orig;
        // input modes: no break, no CR to NL, no parity check, no strip char,
        // no start/stop output control
        raw.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
        // output modes - disable post processing
        raw.c_oflag &= !libc::OPOST;
        // control modes - set 8 bit chars
        raw.c_cflag |= libc::CS8;
        // local modes - echoing off, canonical off, no extended functions,
        // no signal chars (^Z,^C)
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
        // control chars - set return condition: min number of bytes and timer.
        // We want read to return every single byte, without timeout
        raw.c_cc[libc::VMIN] = 1; // 1 byte
        raw.c_cc[libc::VTIME] = 0; // no timer

        if unsafe { tcsetattr(self.ifd, libc::TCSAFLUSH, &raw) } < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut state = G.lock().unwrap();
        state.orig_termios = Some(orig);
        state.raw_mode = true;
        drop(state);

        Ok(RawModeGuard {
            ifd: self.ifd,
            orig_termios: orig,
        })
    }

    fn is_tty(&self) -> bool {
        unsafe { libc::isatty(self.ifd) != 0 }
    }

    fn write(&self, s: &str) -> io::Result<()> {
        self.write_bytes(s.as_bytes())
    }

    fn write_bytes(&self, buf: &[u8]) -> io::Result<()> {
        let mut written = 0;
        while written < buf.len() {
            match unsafe {
                libc::write(
                    self.ofd,
                    buf[written..].as_ptr() as *const c_void,
                    buf.len() - written,
                )
            } {
                -1 => {
                    let err = io::Error::last_os_error();
                    if err.kind() != io::ErrorKind::Interrupted {
                        return Err(err);
                    }
                }
                0 => break,
                n => written += n as usize,
            }
        }
        if self.ofd == libc::STDOUT_FILENO {
            io::stdout().flush()?;
        }
        Ok(())
    }

    fn read_byte(&self) -> io::Result<Option<u8>> {
        let mut c = 0u8;
        loop {
            let n = unsafe { libc::read(self.ifd, &mut c as *mut u8 as *mut c_void, 1) };
            if n == -1 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            return Ok(if n == 0 { None } else { Some(c) });
        }
    }

    fn read_byte_nonblocking(&self) -> io::Result<Option<u8>> {
        use libc::{fcntl, F_GETFL, F_SETFL, O_NONBLOCK};

        unsafe {
            // Get current flags
            let flags = fcntl(self.ifd, F_GETFL, 0);
            if flags == -1 {
                return Err(io::Error::last_os_error());
            }

            // Set non-blocking
            if fcntl(self.ifd, F_SETFL, flags | O_NONBLOCK) == -1 {
                return Err(io::Error::last_os_error());
            }

            // Try to read
            let result = self.read_byte();

            // Restore blocking mode - always attempt this
            let restore_result = fcntl(self.ifd, F_SETFL, flags);

            // Check restore result after we have our read result
            if restore_result == -1 {
                return Err(io::Error::last_os_error());
            }

            match result {
                Ok(b) => Ok(b),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(e),
            }
        }
    }

    /// Use the ESC [6n escape sequence to query the horizontal cursor position
    /// and return it.
    fn get_cursor_position(&self) -> io::Result<(usize, usize)> {
        self.write_bytes(b"\x1b[6n")?;

        let mut buf = [0u8; 32];
        let mut i = 0;

        while i < buf.len() - 1 {
            buf[i] = self.read_byte()?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "EOF reading cursor")
            })?;
            i += 1;
            if buf[i - 1] == b'R' {
                break;
            }
        }

        // Parse response
        let response =
            std::str::from_utf8(&buf[2..i - 1]).map_err(|_| io::Error::other("Invalid UTF-8"))?;

        let (rows, cols) = response
            .split_once(';')
            .ok_or_else(|| io::Error::other("Invalid format"))?;

        Ok((
            rows.parse().map_err(|_| io::Error::other("Invalid row"))?,
            cols.parse().map_err(|_| io::Error::other("Invalid col"))?,
        ))
    }

    /// Try to get the number of columns in the current terminal, or assume 80
    /// if it fails.
    fn get_columns(ifd: RawFd, ofd: RawFd) -> usize {
        // First try with ioctl
        unsafe {
            let mut ws: libc::winsize = mem::zeroed();
            if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col != 0 {
                return ws.ws_col as usize;
            }
        }

        // ioctl() failed. Try to query the terminal itself.
        // This is the fallback method from the original linenoise.

        // We need to create a temporary terminal to use its methods
        let temp_terminal = Terminal { ifd, ofd, cols: 80 };

        // Get the initial position so we can restore it later
        let orig_pos = match temp_terminal.get_cursor_position() {
            Ok(pos) => pos,
            Err(_) => return 80,
        };

        // Go to right margin and get position
        if temp_terminal.write_bytes(b"\x1b[999C").is_err() {
            return 80;
        }

        let cols = match temp_terminal.get_cursor_position() {
            Ok(pos) => pos.1,
            Err(_) => 80,
        };

        // Restore position
        if orig_pos != (0, 0) {
            let _ = temp_terminal.write(&format!("\x1b[{};{}H", orig_pos.0, orig_pos.1));
        }

        cols
    }

    fn clear_screen(&self) -> io::Result<()> {
        self.write("\x1b[H\x1b[2J")
    }

    fn beep(&self) {
        let _ = self.write("\x07");
    }
}

// Line buffer for editing
struct LineBuffer {
    chars: Vec<char>,
    pos: usize,
}

impl LineBuffer {
    fn new() -> Self {
        LineBuffer {
            chars: Vec::with_capacity(LINENOISE_MAX_LINE),
            pos: 0,
        }
    }

    fn insert(&mut self, c: char) -> bool {
        if self.chars.len() >= LINENOISE_MAX_LINE - 1 {
            return false;
        }
        self.chars.insert(self.pos, c);
        self.pos += 1;
        true
    }

    fn delete(&mut self) -> bool {
        if self.pos < self.chars.len() {
            self.chars.remove(self.pos);
            true
        } else {
            false
        }
    }

    fn backspace(&mut self) -> bool {
        if self.pos > 0 {
            self.pos -= 1;
            self.chars.remove(self.pos);
            true
        } else {
            false
        }
    }

    fn move_left(&mut self) -> bool {
        if self.pos > 0 {
            self.pos -= 1;
            true
        } else {
            false
        }
    }

    fn move_right(&mut self) -> bool {
        if self.pos < self.chars.len() {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn move_home(&mut self) {
        self.pos = 0;
    }

    fn move_end(&mut self) {
        self.pos = self.chars.len();
    }

    fn delete_to_end(&mut self) {
        self.chars.truncate(self.pos);
    }

    fn delete_word(&mut self) {
        let start = self.pos;

        // Skip spaces
        while self.pos > 0 && self.chars[self.pos - 1] == ' ' {
            self.pos -= 1;
        }

        // Skip word
        while self.pos > 0 && self.chars[self.pos - 1] != ' ' {
            self.pos -= 1;
        }

        self.chars.drain(self.pos..start);
    }

    fn clear(&mut self) {
        self.chars.clear();
        self.pos = 0;
    }

    fn set(&mut self, s: &str) {
        self.chars = s.chars().take(LINENOISE_MAX_LINE - 1).collect();
        self.pos = self.chars.len();
    }

    fn as_string(&self) -> String {
        self.chars.iter().collect()
    }
}

struct Editor {
    terminal: Terminal,
    buffer: LineBuffer,
    prompt: String,
    history_index: usize,
    saved_line: Option<String>,
    completion_state: Option<CompletionState>,
    old_rows: usize,          // For multiline mode
    cursor_row_offset: usize, // For multiline mode
}

struct CompletionState {
    original_line: String,
    current_index: usize,
}

// Helper macro for common key processing pattern
macro_rules! key_action {
    ($self:expr, $action:expr) => {{
        $action;
        $self.refresh_line()?;
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "More input needed",
        ))
    }};
}

impl Editor {
    fn new(terminal: Terminal, prompt: &str) -> Self {
        Editor {
            terminal,
            buffer: LineBuffer::new(),
            prompt: prompt.to_string(),
            history_index: 0,
            saved_line: None,
            completion_state: None,
            old_rows: 0,
            cursor_row_offset: 0,
        }
    }

    fn refresh_line(&mut self) -> io::Result<()> {
        // Update terminal columns in case of resize
        self.terminal.cols = Terminal::get_columns(self.terminal.ifd, self.terminal.ofd);

        let state = G.lock().unwrap();

        if state.multi_line {
            self.refresh_multiline(&state)
        } else {
            self.refresh_singleline(&state)
        }
    }

    fn refresh_singleline(&mut self, state: &GlobalState) -> io::Result<()> {
        let mut output = String::new();

        // Move to start of line
        output.push('\r');

        // Write prompt
        output.push_str(&self.prompt);

        // Write buffer content
        let content = if state.mask_mode {
            "*".repeat(self.buffer.chars.len())
        } else {
            self.buffer.as_string()
        };

        // Handle line that's too long
        let prompt_len = self.prompt.chars().count();
        let available_cols = self.terminal.cols.saturating_sub(prompt_len);

        let cursor_screen_pos = if content.chars().count() > available_cols {
            // Show a window around the cursor
            let window_start = self.buffer.pos.saturating_sub(available_cols / 2);
            let window_end = min(window_start + available_cols, content.chars().count());
            let actual_window_start = window_end.saturating_sub(available_cols);

            let window: String = content
                .chars()
                .skip(actual_window_start)
                .take(available_cols)
                .collect();
            output.push_str(&window);

            // Calculate cursor position within the window
            prompt_len + self.buffer.pos.saturating_sub(actual_window_start)
        } else {
            output.push_str(&content);

            // Add hints if available (but not during completion)
            if self.completion_state.is_none() {
                if let Some(ref callback) = state.hints_callback {
                    if let Some((hint, color, bold)) = callback(&self.buffer.as_string()) {
                        let remaining = available_cols.saturating_sub(content.chars().count());
                        if remaining > 0 {
                            if bold {
                                output.push_str("\x1b[1m");
                            }
                            if color >= 0 {
                                output.push_str(&format!("\x1b[{color}m"));
                            }
                            let hint_truncated: String = hint.chars().take(remaining).collect();
                            output.push_str(&hint_truncated);
                            output.push_str("\x1b[0m");
                        }
                    }
                }
            }

            // When not windowing, cursor position is trivial
            prompt_len + self.buffer.pos
        };

        // Clear to end of line
        output.push_str("\x1b[0K");

        // Position cursor
        output.push_str(&format!("\r\x1b[{cursor_screen_pos}C"));

        self.terminal.write(&output)
    }

    fn refresh_multiline(&mut self, state: &GlobalState) -> io::Result<()> {
        let mut output = String::new();
        let plen = self.prompt.chars().count();
        let cols = self.terminal.cols;

        // Calculate dimensions
        let content_len = plen + self.buffer.chars.len();
        let cursor_pos = plen + self.buffer.pos;

        // Calculate how many rows we need
        let content_rows = if content_len == 0 {
            1
        } else {
            (content_len + cols - 1) / cols
        };

        // Do we need an extra row for cursor at end of line?
        let phantom_line =
            self.buffer.pos == self.buffer.chars.len() && cursor_pos > 0 && cursor_pos % cols == 0;

        let total_rows = if phantom_line {
            content_rows + 1
        } else {
            content_rows
        };

        // Calculate where cursor should be
        let cursor_row = if cursor_pos == 0 {
            0
        } else if phantom_line {
            content_rows // 0-indexed, so this is the phantom line
        } else {
            (cursor_pos - 1) / cols
        };

        let cursor_col = if phantom_line {
            0
        } else if cursor_pos == 0 {
            plen
        } else {
            (cursor_pos - 1) % cols + 1
        };

        // Move cursor to start of edit area
        output.push('\r');

        // Then move up by our tracked offset
        if self.cursor_row_offset > 0 {
            output.push_str(&format!("\x1b[{}A", self.cursor_row_offset));
        }

        // Now clear everything
        let rows_to_clear = self.old_rows.max(total_rows);
        for i in 0..rows_to_clear {
            if i > 0 {
                output.push_str("\r\n"); // New line
            }
            output.push_str("\x1b[2K"); // Clear entire line
        }

        // Go back to start
        if rows_to_clear > 1 {
            output.push_str(&format!("\x1b[{}A", rows_to_clear - 1));
        }
        output.push('\r');

        // Write content
        output.push_str(&self.prompt);
        if state.mask_mode {
            output.push_str(&"*".repeat(self.buffer.chars.len()));
        } else {
            output.push_str(&self.buffer.as_string());
        }

        // Add hints if appropriate
        if content_rows == 1 && !phantom_line && self.completion_state.is_none() {
            if let Some(ref cb) = state.hints_callback {
                if let Some((hint, color, bold)) = cb(&self.buffer.as_string()) {
                    let last_line_len = content_len % cols;
                    let space = if last_line_len == 0 {
                        0
                    } else {
                        cols - last_line_len
                    };

                    if space > 0 {
                        let hint_str: String = hint.chars().take(space).collect();
                        if !hint_str.is_empty() {
                            if bold {
                                output.push_str("\x1b[1m");
                            }
                            if color >= 0 {
                                output.push_str(&format!("\x1b[{color}m"));
                            }
                            output.push_str(&hint_str);
                            output.push_str("\x1b[0m");
                        }
                    }
                }
            }
        }

        // Add phantom line if needed
        if phantom_line {
            output.push_str("\r\n");
        }

        // Now position cursor
        // We're currently at end of content
        let current_row = total_rows - 1;

        // Move to cursor row
        if cursor_row < current_row {
            output.push_str(&format!("\x1b[{}A", current_row - cursor_row));
        } else if cursor_row > current_row {
            output.push_str(&format!("\x1b[{}B", cursor_row - current_row));
        }

        // Move to cursor column
        output.push_str(&format!("\r\x1b[{}C", cursor_col));

        // Update state
        self.old_rows = total_rows;
        self.cursor_row_offset = cursor_row;

        self.terminal.write(&output)
    }

    fn handle_completion(&mut self) -> io::Result<bool> {
        // Get the completion callback
        let callback = {
            let state = G.lock().unwrap();
            state.completion_callback
        };

        let Some(cb) = callback else {
            return Ok(false);
        };

        // Determine which line to use for completion
        let line_for_completion = if let Some(ref comp_state) = self.completion_state {
            // Use the original line saved when completion started
            comp_state.original_line.clone()
        } else {
            // First tab - use current buffer
            self.buffer.as_string()
        };

        // Get completions
        let mut completions = Vec::new();
        cb(&line_for_completion, &mut completions);

        if completions.is_empty() {
            self.terminal.beep();
            self.completion_state = None;
            return Ok(false);
        }

        // Update completion state
        if let Some(ref mut comp_state) = self.completion_state {
            // Already in completion mode - cycle to next
            comp_state.current_index = (comp_state.current_index + 1) % completions.len();

            if let Some(completion) = completions.get(comp_state.current_index) {
                self.buffer.set(completion);
                self.refresh_line()?;
            }
        } else {
            // First tab - start completion mode
            self.completion_state = Some(CompletionState {
                original_line: line_for_completion,
                current_index: 0,
            });

            // Show first completion
            if let Some(first) = completions.first() {
                self.buffer.set(first);
                self.refresh_line()?;
            }
        }

        Ok(true)
    }

    fn accept_completion(&mut self) {
        self.completion_state = None;
    }

    fn handle_history(&mut self, direction: isize) -> io::Result<()> {
        let state = G.lock().unwrap();
        let history_len = state.history.entries.len();

        if history_len == 0 {
            return Ok(());
        }

        // Save current line on first history access
        if self.history_index == 0 && self.saved_line.is_none() {
            self.saved_line = Some(self.buffer.as_string());
        }

        // Update history index
        if direction > 0 {
            if self.history_index < history_len {
                self.history_index += 1;
            }
        } else if self.history_index > 0 {
            self.history_index -= 1;
        }

        // Load history entry or restore saved line
        if self.history_index == 0 {
            if let Some(saved) = &self.saved_line {
                self.buffer.set(saved);
            }
        } else if let Some(entry) = state.history.get(self.history_index) {
            self.buffer.set(entry);
        }

        drop(state);
        self.refresh_line()
    }

    fn handle_escape_sequence(&mut self) -> io::Result<()> {
        let seq = [
            self.terminal.read_byte_nonblocking()?,
            self.terminal.read_byte_nonblocking()?,
        ];

        let action: Option<fn(&mut Self) -> io::Result<()>> = match seq {
            [Some(b'['), Some(b'A')] => Some(|s| s.handle_history(1)),
            [Some(b'['), Some(b'B')] => Some(|s| s.handle_history(-1)),
            [Some(b'['), Some(b'C')] => Some(|s| {
                s.buffer.move_right();
                Ok(())
            }),
            [Some(b'['), Some(b'D')] => Some(|s| {
                s.buffer.move_left();
                Ok(())
            }),
            [Some(b'['), Some(b'H')] | [Some(b'O'), Some(b'H')] => Some(|s| {
                s.buffer.move_home();
                Ok(())
            }),
            [Some(b'['), Some(b'F')] | [Some(b'O'), Some(b'F')] => Some(|s| {
                s.buffer.move_end();
                Ok(())
            }),
            _ => None,
        };

        if let Some(f) = action {
            f(self)?;
            self.refresh_line()?;
        }

        // Special case for delete
        if matches!(seq, [Some(b'['), Some(b'3')])
            && matches!(self.terminal.read_byte_nonblocking()?, Some(b'~'))
            && self.buffer.delete()
        {
            self.refresh_line()?;
        }

        Ok(())
    }

    /// Process a single input character/byte
    fn process_key(&mut self, c: u8) -> io::Result<Option<String>> {
        // Handle completion state
        if self.completion_state.is_some() && c != Key::Tab as u8 {
            self.accept_completion();
        }

        match c {
            c if c == Key::Enter as u8 => Ok(Some(self.buffer.as_string())),
            c if c == Key::CtrlC as u8 => Err(io::Error::new(io::ErrorKind::Interrupted, "")),
            c if c == Key::CtrlD as u8 => {
                if self.buffer.chars.is_empty() {
                    Ok(None)
                } else {
                    if self.buffer.delete() {
                        self.refresh_line()?;
                    }
                    Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "More input needed",
                    ))
                }
            }
            c if c == Key::Tab as u8 => {
                self.handle_completion()?;
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "More input needed",
                ))
            }
            c if c == Key::Backspace as u8 || c == Key::CtrlH as u8 => key_action!(self, {
                self.buffer.backspace();
            }),
            c if c == Key::CtrlU as u8 => key_action!(self, self.buffer.clear()),
            c if c == Key::CtrlK as u8 => key_action!(self, self.buffer.delete_to_end()),
            c if c == Key::CtrlW as u8 => key_action!(self, self.buffer.delete_word()),
            c if c == Key::CtrlA as u8 => key_action!(self, self.buffer.move_home()),
            c if c == Key::CtrlE as u8 => key_action!(self, self.buffer.move_end()),
            c if c == Key::CtrlB as u8 => key_action!(self, {
                self.buffer.move_left();
            }),
            c if c == Key::CtrlF as u8 => key_action!(self, {
                self.buffer.move_right();
            }),
            c if c == Key::CtrlP as u8 => {
                self.handle_history(1)?;
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "More input needed",
                ))
            }
            c if c == Key::CtrlN as u8 => {
                self.handle_history(-1)?;
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "More input needed",
                ))
            }
            c if c == Key::CtrlL as u8 => {
                self.terminal.clear_screen()?;
                self.old_rows = 0;
                self.cursor_row_offset = 0;
                self.refresh_line()?;
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "More input needed",
                ))
            }
            c if c == Key::CtrlT as u8 => {
                // Transpose chars
                if self.buffer.pos > 0 && self.buffer.chars.len() > 1 {
                    if self.buffer.pos == self.buffer.chars.len() {
                        self.buffer
                            .chars
                            .swap(self.buffer.pos - 2, self.buffer.pos - 1);
                    } else {
                        self.buffer.chars.swap(self.buffer.pos - 1, self.buffer.pos);
                        self.buffer.pos += 1;
                    }
                    self.refresh_line()?;
                }
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "More input needed",
                ))
            }
            c if c == Key::Esc as u8 => {
                self.handle_escape_sequence()?;
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "More input needed",
                ))
            }
            c if c >= 32 && c < 127 => {
                // Printable ASCII
                if self.buffer.insert(c as char) {
                    self.refresh_line()?;
                } else {
                    self.terminal.beep();
                }
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "More input needed",
                ))
            }
            c if c >= 128 => {
                // UTF-8 handling
                let mut utf8_buf = vec![c];

                // Determine how many bytes we need
                let bytes_needed = if c & 0xE0 == 0xC0 {
                    2
                } else if c & 0xF0 == 0xE0 {
                    3
                } else if c & 0xF8 == 0xF0 {
                    4
                } else {
                    1 // Invalid UTF-8 start byte
                };

                // Read remaining bytes
                for _ in 1..bytes_needed {
                    if let Ok(Some(next_byte)) = self.terminal.read_byte() {
                        if next_byte & 0xC0 == 0x80 {
                            utf8_buf.push(next_byte);
                        } else {
                            break; // Invalid continuation byte
                        }
                    } else {
                        break;
                    }
                }

                // Try to decode
                if utf8_buf.len() == bytes_needed {
                    if let Ok(s) = std::str::from_utf8(&utf8_buf) {
                        if let Some(ch) = s.chars().next() {
                            if self.buffer.insert(ch) {
                                self.refresh_line()?;
                            } else {
                                self.terminal.beep();
                            }
                        }
                    }
                } else {
                    self.terminal.beep();
                }
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "More input needed",
                ))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "More input needed",
            )),
        }
    }
}

/// Return true if the terminal name is in the list of terminals we know
/// are not able to understand basic escape sequences.
fn is_unsupported_term() -> bool {
    let unsupported = ["dumb", "cons25", "emacs"];
    if let Ok(term) = env::var("TERM") {
        unsupported.iter().any(|&t| term.eq_ignore_ascii_case(t))
    } else {
        false
    }
}

// Public API

/// The high level function that is the main API of the linenoise library.
/// This function checks if the terminal has basic capabilities, just checking
/// for a blacklist of stupid terminals, and later either calls the line editing
/// function or uses dummy `fgets()` so that you will be able to type something
/// even in the most desperate of conditions.
pub fn linenoise(prompt: &str) -> Option<String> {
    let terminal = Terminal::new(libc::STDIN_FILENO, libc::STDOUT_FILENO);

    if !terminal.is_tty() {
        return linenoise_no_tty();
    }

    if is_unsupported_term() {
        return linenoise_unsupported_term(prompt);
    }

    // Use the multiplexed API internally
    let mut state = match LinenoiseState::edit_start(-1, -1, prompt) {
        Ok(s) => s,
        Err(_) => return None,
    };

    // Read until we get a result
    loop {
        match state.edit_feed() {
            Ok(Some(line)) => {
                let _ = state.edit_stop();
                return Some(line);
            }
            Ok(None) => {
                let _ = state.edit_stop();
                return None;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                let _ = state.edit_stop();
                return None;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Need more input, continue
                continue;
            }
            Err(_) => {
                let _ = state.edit_stop();
                return None;
            }
        }
    }
}

/// For when we are not a TTY
fn linenoise_no_tty() -> Option<String> {
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) => None, // EOF
        Ok(_) => {
            // Remove trailing newline
            if line.ends_with('\n') {
                line.pop();
                if line.ends_with('\r') {
                    line.pop();
                }
            }
            Some(line)
        }
        Err(_) => None,
    }
}

/// For unsupported terminals provide basic functionality
fn linenoise_unsupported_term(prompt: &str) -> Option<String> {
    print!("{prompt}");
    let _ = io::stdout().flush();

    linenoise_no_tty()
}

/// Toggle multi line mode.
pub fn linenoise_set_multi_line(ml: bool) {
    G.lock().unwrap().multi_line = ml;
}

/// Enable mask mode. When it is enabled, instead of the input that
/// the user is typing, the terminal will just display a corresponding
/// number of asterisks, like "***". This is useful for passwords and
/// other secrets that should not be displayed.
pub fn linenoise_mask_mode_enable() {
    G.lock().unwrap().mask_mode = true;
}

/// Disable mask mode.
pub fn linenoise_mask_mode_disable() {
    G.lock().unwrap().mask_mode = false;
}

/// Register a callback function to be called for tab-completion.
pub fn linenoise_set_completion_callback(cb: CompletionCallback) {
    G.lock().unwrap().completion_callback = Some(cb);
}

/// Registers a hints function to be called to show hints to the user
/// at the right of the prompt.
pub fn linenoise_set_hints_callback(cb: HintsCallback) {
    G.lock().unwrap().hints_callback = Some(cb);
}

/// This is the API call to add a new entry to the linenoise history.
pub fn linenoise_history_add(line: &str) -> bool {
    G.lock().unwrap().history.add(line)
}

/// Set the maximum length for the history. This function can be called
/// even if there is already some history, the function will make sure
/// to retain just the latest `len` elements if the new history length
/// value is smaller than the amount of items already inside the history.
pub fn linenoise_history_set_max_len(len: usize) -> bool {
    if len < 1 {
        return false;
    }
    let mut state = G.lock().unwrap();
    state.history.max_len = len;
    while state.history.entries.len() > len {
        state.history.entries.pop_front();
    }
    true
}

/// Save the history to the specified file.
pub fn linenoise_history_save(filename: &str) -> io::Result<()> {
    let state = G.lock().unwrap();
    let mut file = File::create(filename)?;
    for entry in &state.history.entries {
        writeln!(file, "{entry}")?;
    }
    Ok(())
}

/// Load the history from the specified file. If the file does not exist
/// then no operation is performed.
///
/// If file exists then it returns `Ok()` on success and an error on fail.
pub fn linenoise_history_load(filename: &str) -> io::Result<()> {
    let file = match File::open(filename) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };

    let reader = BufReader::new(file);
    let mut state = G.lock().unwrap();

    #[allow(clippy::manual_flatten)]
    for line in reader.lines() {
        if let Ok(line) = line {
            let trimmed = line.trim_end();
            if !trimmed.is_empty() {
                state.history.add(trimmed);
            }
        }
    }

    Ok(())
}

/// Clear the screen. Used to handle Ctrl+L
pub fn linenoise_clear_screen() {
    let terminal = Terminal::new(libc::STDIN_FILENO, libc::STDOUT_FILENO);
    let _ = terminal.clear_screen();
}

/// This special mode is used by linenoise in order to print scan codes
/// on screen for debugging/development purposes. It is implemented by
/// the linenoise example program using the `--keycodes` option.
pub fn linenoise_print_key_codes() {
    let terminal = Terminal::new(libc::STDIN_FILENO, libc::STDOUT_FILENO);

    println!("Linenoise key codes debugging mode.");
    println!("Press keys to see scan codes. Type 'quit' to exit.");

    let _guard = match terminal.enable_raw_mode() {
        Ok(guard) => guard,
        Err(_) => return,
    };

    let mut quit_buf = [0u8; 4];

    loop {
        if let Ok(Some(c)) = terminal.read_byte() {
            // Shift buffer
            quit_buf[0] = quit_buf[1];
            quit_buf[1] = quit_buf[2];
            quit_buf[2] = quit_buf[3];
            quit_buf[3] = c;

            // Build the output string first to avoid any control char interpretation
            let mut output = format!(
                "'{}'  {:#04x}",
                if c >= 32 && c < 127 { c as char } else { '?' },
                c
            );

            // Append name if known
            match c {
                1 => output.push_str(" (ctrl-a)"),
                2 => output.push_str(" (ctrl-b)"),
                3 => output.push_str(" (ctrl-c)"),
                4 => output.push_str(" (ctrl-d)"),
                5 => output.push_str(" (ctrl-e)"),
                6 => output.push_str(" (ctrl-f)"),
                8 => output.push_str(" (ctrl-h)"),
                9 => output.push_str(" (tab)"),
                11 => output.push_str(" (ctrl-k)"),
                12 => output.push_str(" (ctrl-l)"),
                13 => output.push_str(" (enter)"),
                14 => output.push_str(" (ctrl-n)"),
                16 => output.push_str(" (ctrl-p)"),
                20 => output.push_str(" (ctrl-t)"),
                21 => output.push_str(" (ctrl-u)"),
                23 => output.push_str(" (ctrl-w)"),
                27 => output.push_str(" (esc)"),
                127 => output.push_str(" (backspace)"),
                _ => {}
            }

            // Use write to avoid println's processing, add explicit \r\n
            let _ = terminal.write(&format!("{}\r\n", output));

            if &quit_buf == b"quit" {
                break;
            }
        }
    }

    // _guard will be dropped here, terminal returns to cooked mode
    println!();
}

/// Multiplexing support
pub struct LinenoiseState {
    editor: Editor,
    active: bool,
    _raw_guard: Option<RawModeGuard>,
}

impl LinenoiseState {
    /// This function is part of the multiplexed API in linenoise, that is used in order
    /// to implement the blocking variant of the API but can also be called by the user
    /// directly in an event-driven program.
    pub fn edit_start(stdin_fd: RawFd, stdout_fd: RawFd, prompt: &str) -> io::Result<Self> {
        let ifd = if stdin_fd == -1 {
            libc::STDIN_FILENO
        } else {
            stdin_fd
        };

        let ofd = if stdout_fd == -1 {
            libc::STDOUT_FILENO
        } else {
            stdout_fd
        };

        let terminal = Terminal::new(ifd, ofd);

        if !terminal.is_tty() || is_unsupported_term() {
            return Err(io::Error::other("Not supported"));
        }

        let raw_guard = terminal.enable_raw_mode()?;

        let mut editor = Editor::new(terminal, prompt);

        // Reset editor state for new session
        editor.history_index = 0;
        editor.saved_line = None;

        // Display initial prompt
        editor.refresh_line()?;

        Ok(Self {
            editor,
            active: true,
            _raw_guard: Some(raw_guard),
        })
    }

    /// Part of the multiplexed API. Call this function each time there is some data
    /// to read from the standard input file descriptor. In case of blocking operations
    /// this function can just be called in a loop, and block.
    pub fn edit_feed(&mut self) -> io::Result<Option<String>> {
        if !self.active {
            return Ok(None);
        }

        // Try to read a byte
        match self.editor.terminal.read_byte()? {
            Some(c) => {
                match self.editor.process_key(c) {
                    Ok(result) => {
                        if result.is_some() {
                            // Move to new line before returning
                            let _ = self.editor.terminal.write("\r\n");
                            self.active = false;
                        }
                        Ok(result)
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                        self.active = false;
                        Err(e)
                    }
                    Err(e) => Err(e),
                }
            }
            None => {
                // EOF
                self.active = false;
                Ok(None)
            }
        }
    }

    /// Part of the multiplexed API. At this point the user input is in the buffer,
    /// and we can restore the terminal in normal node.
    pub fn edit_stop(&mut self) -> io::Result<()> {
        if self.active {
            self.active = false;
            // Drop the guard to restore terminal
            self._raw_guard = None;
        }
        Ok(())
    }

    /// Hide the current line, when using the multiplexed API.
    pub fn hide(&self) -> io::Result<()> {
        // Move to beginning of line and clear it
        self.editor.terminal.write("\r\x1b[0K")
    }

    /// Show the current line, when using the multiplexed API.
    pub fn show(&mut self) -> io::Result<()> {
        // Instead of restoring cursor position which might be stale,
        // just move to beginning of line and refresh
        self.editor.terminal.write("\r")?;
        self.editor.refresh_line()
    }

    pub fn get_fd(&self) -> RawFd {
        self.editor.terminal.ifd
    }
}
