//! OSC escape sequence interceptor for sequences not handled by alacritty_terminal.
//!
//! This module pre-parses the PTY output stream to extract OSC sequences that
//! alacritty_terminal does not expose as events:
//! - OSC 7: Working directory (file:// URL)
//! - OSC 9;4: Progress indicator (ConEmu/Windows Terminal)
//! - OSC 9;9: Working directory (ConEmu path form; WSL reports its cwd this way)
//! - OSC 133: Shell integration (prompt/command lifecycle)

use crate::shell_integration::ProgressState;
use std::borrow::Cow;

/// Events extracted from custom OSC sequences
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscEvent {
    /// OSC 7 / OSC 9;9 - Working directory change
    /// Formats: ESC ] 7 ; file://hostname/path ST
    ///          ESC ] 9 ; 9 ; path ST (path may be double-quoted; emitted by WSL)
    WorkingDirectory(String),

    /// OSC 9;4 - Progress indicator (ConEmu/Windows Terminal)
    /// Format: ESC ] 9 ; 4 ; state ; progress ST
    Progress(ProgressState),

    /// OSC 9;9 - ConPTY/cmd/WSL 在每个提示符前上报 cwd，视为"shell 回到提示符"的信号。
    /// 只随 OSC 9;9 附带发出（紧跟在对应的 `WorkingDirectory` 之后），OSC 7 不触发，
    /// 避免运行中会自报 cwd 的 TUI 被误判成空闲。
    PromptIdle,

    /// OSC 133;A - Prompt start
    ShellPromptStart,

    /// OSC 133;B - Command input start
    ShellCommandStart,

    /// OSC 133;C - Command executing
    ShellCommandExecuting,

    /// OSC 133;D - Command finished with optional exit code
    ShellCommandFinished(Option<i32>),
}

/// State machine for parsing OSC sequences from a byte stream.
///
/// The interceptor processes bytes and extracts custom OSC sequences,
/// passing through all other data unchanged.
#[derive(Debug, Default)]
pub struct OscInterceptor {
    /// Buffer for accumulating OSC payload
    buffer: Vec<u8>,
    /// Current parsing state
    state: ParseState,
}

/// Maximum size for the OSC buffer to prevent unbounded memory growth from malformed input.
const MAX_OSC_BUFFER: usize = 64 * 1024; // 64 KB

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    /// Normal passthrough mode
    #[default]
    Ground,
    /// Saw ESC (0x1B)
    Escape,
    /// Saw ESC ] (OSC start)
    OscStart,
    /// Accumulating OSC payload
    OscPayload,
    /// Saw ESC within OSC (possible ST)
    OscEscape,
}

impl OscInterceptor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process input bytes, extracting custom OSC sequences.
    ///
    /// Returns a tuple of:
    /// - Filtered bytes (with extracted OSC sequences removed)
    /// - Vector of extracted OSC events
    pub fn process<'a>(&mut self, input: &'a [u8]) -> (Cow<'a, [u8]>, Vec<OscEvent>) {
        // PTY output is overwhelmingly ordinary text and CSI control traffic.
        // Avoid copying and walking those chunks byte-by-byte when there cannot
        // be an OSC sequence for this interceptor to handle. A trailing ESC
        // must stay on the state-machine path because the following `]` can
        // arrive in the next read.
        if self.state == ParseState::Ground && !input_requires_interception(input) {
            return (Cow::Borrowed(input), Vec::new());
        }

        let mut output = Vec::with_capacity(input.len());
        let mut events = Vec::new();

        for &byte in input {
            match self.state {
                ParseState::Ground => {
                    if byte == 0x1B {
                        // ESC
                        self.state = ParseState::Escape;
                    } else {
                        output.push(byte);
                    }
                }

                ParseState::Escape => {
                    if byte == b']' {
                        // ESC ] = OSC start
                        self.state = ParseState::OscStart;
                        self.buffer.clear();
                    } else {
                        // Not an OSC, pass through ESC and this byte
                        output.push(0x1B);
                        output.push(byte);
                        self.state = ParseState::Ground;
                    }
                }

                ParseState::OscStart => {
                    // First byte after ESC ] determines if we care about this OSC
                    self.buffer.push(byte);
                    self.state = ParseState::OscPayload;
                }

                ParseState::OscPayload => {
                    if byte == 0x07 {
                        // BEL = OSC terminator
                        if self.collect_events(&mut events) {
                            // 自定义 OSC 已转成事件，不再透传
                        } else if self.should_consume_osc_payload() {
                            // Removed notification OSCs are handled by dropping them.
                        } else {
                            // Not a custom OSC, pass it through
                            self.emit_osc_to_output(&mut output);
                        }
                        self.buffer.clear();
                        self.state = ParseState::Ground;
                    } else if byte == 0x1B {
                        // ESC within OSC - might be ST (ESC \)
                        self.state = ParseState::OscEscape;
                    } else if self.buffer.len() < MAX_OSC_BUFFER {
                        self.buffer.push(byte);
                    } else {
                        // Buffer overflow - emit partial OSC and reset
                        self.emit_osc_to_output(&mut output);
                        self.buffer.clear();
                        self.state = ParseState::Ground;
                    }
                }

                ParseState::OscEscape => {
                    if byte == b'\\' {
                        // ESC \ = ST (String Terminator)
                        if self.collect_events(&mut events) {
                            // 自定义 OSC 已转成事件，不再透传
                        } else if self.should_consume_osc_payload() {
                            // Removed notification OSCs are handled by dropping them.
                        } else {
                            // Not a custom OSC, pass it through
                            self.emit_osc_to_output(&mut output);
                        }
                        self.buffer.clear();
                        self.state = ParseState::Ground;
                    } else if self.buffer.len() + 2 <= MAX_OSC_BUFFER {
                        // Not ST, add ESC to buffer and continue
                        self.buffer.push(0x1B);
                        self.buffer.push(byte);
                        self.state = ParseState::OscPayload;
                    } else {
                        // Buffer overflow - emit partial OSC and reset
                        self.emit_osc_to_output(&mut output);
                        self.buffer.clear();
                        self.state = ParseState::Ground;
                    }
                }
            }
        }

        (Cow::Owned(output), events)
    }

    /// Emit the current OSC buffer as passthrough to output
    fn emit_osc_to_output(&self, output: &mut Vec<u8>) {
        output.push(0x1B);
        output.push(b']');
        output.extend_from_slice(&self.buffer);
        output.push(0x07);
    }

    fn should_consume_osc_payload(&self) -> bool {
        let Ok(payload) = std::str::from_utf8(&self.buffer) else {
            return false;
        };

        payload.starts_with("9;") || payload.starts_with("777;")
    }

    /// 把当前缓冲的 OSC 解析成自定义事件追加到 `events`；识别成功返回 true。
    ///
    /// OSC 9;9 会在 `WorkingDirectory` 之后额外追加 `PromptIdle`。
    fn collect_events(&self, events: &mut Vec<OscEvent>) -> bool {
        let Some(event) = self.parse_osc_payload() else {
            return false;
        };
        let prompt_idle =
            matches!(event, OscEvent::WorkingDirectory(_)) && self.buffer.starts_with(b"9;9;");
        events.push(event);
        if prompt_idle {
            events.push(OscEvent::PromptIdle);
        }
        true
    }

    /// Try to parse the OSC payload as a custom sequence we handle
    fn parse_osc_payload(&self) -> Option<OscEvent> {
        let payload = std::str::from_utf8(&self.buffer).ok()?;

        // OSC 7 - Working directory
        if let Some(url) = payload.strip_prefix("7;") {
            return Some(OscEvent::WorkingDirectory(
                parse_file_url(url).unwrap_or_else(|| url.to_string()),
            ));
        }

        // OSC 9;4;state;progress - Progress indicator
        if let Some(rest) = payload.strip_prefix("9;") {
            if let Some(progress_part) = rest.strip_prefix("4;") {
                return parse_progress(progress_part);
            }
            // OSC 9;9;path - ConEmu working directory report; WSL emits the
            // Windows-visible cwd (e.g. \\wsl.localhost\...) this way, optionally
            // wrapped in double quotes.
            if let Some(cwd) = rest.strip_prefix("9;") {
                let cwd = cwd.trim().trim_matches('"');
                if !cwd.is_empty() {
                    return Some(OscEvent::WorkingDirectory(cwd.to_string()));
                }
            }
            return None;
        }

        // OSC 133 - Shell integration
        if let Some(rest) = payload.strip_prefix("133;") {
            return parse_shell_integration(rest);
        }

        None
    }
}

fn input_requires_interception(input: &[u8]) -> bool {
    if input.last() == Some(&0x1B) {
        return true;
    }

    input
        .windows(2)
        .any(|pair| pair[0] == 0x1B && pair[1] == b']')
}

/// 解析 OSC 7 的 `file://host/path` URL，返回已解码 `%XX` 的路径；不是 file URL 返回 None。
///
/// 只有 OSC 7 是 URL 形式，才在这里解码；OSC 9;9 的原样路径不经过此函数，字面 `%XX` 保持不变。
fn parse_file_url(url: &str) -> Option<String> {
    // Format: file://hostname/path or file:///path
    let rest = url.strip_prefix("file://")?;
    // 跳过主机名（第一个 / 之前的部分）。
    let slash_pos = rest.find('/')?;
    Some(
        percent_encoding::percent_decode_str(&rest[slash_pos..])
            .decode_utf8_lossy()
            .into_owned(),
    )
}

/// Parse OSC 9;4 progress indicator
fn parse_progress(payload: &str) -> Option<OscEvent> {
    // Format: state;progress
    let mut parts = payload.split(';');
    let state: u8 = parts.next()?.parse().ok()?;
    let progress: u8 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Some(OscEvent::Progress(ProgressState::from_osc(state, progress)))
}

/// Parse OSC 133 shell integration sequences
fn parse_shell_integration(payload: &str) -> Option<OscEvent> {
    // Format: A, B, C, or D;exit_code
    match payload.chars().next()? {
        'A' => Some(OscEvent::ShellPromptStart),
        'B' => Some(OscEvent::ShellCommandStart),
        'C' => Some(OscEvent::ShellCommandExecuting),
        'D' => {
            // D may be followed by ;exit_code
            let exit_code = payload
                .strip_prefix("D;")
                .and_then(|s| s.parse::<i32>().ok());
            Some(OscEvent::ShellCommandFinished(exit_code))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process_str(interceptor: &mut OscInterceptor, s: &str) -> (String, Vec<OscEvent>) {
        let (bytes, events) = interceptor.process(s.as_bytes());
        (String::from_utf8_lossy(&bytes).into_owned(), events)
    }

    #[test]
    fn borrows_plain_and_csi_only_input() {
        let mut interceptor = OscInterceptor::new();

        let (plain, plain_events) = interceptor.process(b"hello world");
        assert!(matches!(plain, Cow::Borrowed(_)));
        assert!(plain_events.is_empty());

        let (csi, csi_events) = interceptor.process(b"\x1b[Hhello\x1b[2J");
        assert!(matches!(csi, Cow::Borrowed(_)));
        assert!(csi_events.is_empty());
    }

    #[test]
    fn keeps_trailing_escape_on_state_machine_path() {
        let mut interceptor = OscInterceptor::new();

        let (prefix, prefix_events) = interceptor.process(b"hello\x1b");
        assert!(matches!(prefix, Cow::Owned(_)));
        assert_eq!(prefix.as_ref(), b"hello");
        assert!(prefix_events.is_empty());

        let (suffix, suffix_events) = interceptor.process(b"]133;A\x07world");
        assert_eq!(suffix.as_ref(), b"world");
        assert_eq!(suffix_events, vec![OscEvent::ShellPromptStart]);
    }

    #[test]
    fn every_chunk_split_matches_one_shot_processing() {
        let inputs: &[&[u8]] = &[
            b"plain text\x1b[H with csi\x1b[2J",
            b"before\x1b]133;A\x07after",
            b"before\x1b]0;window title\x1b\\after",
            b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
        ];

        for input in inputs {
            let mut one_shot = OscInterceptor::new();
            let (expected_output, expected_events) = one_shot.process(input);

            for split in 0..=input.len() {
                let mut chunked = OscInterceptor::new();
                let (first_output, mut events) = chunked.process(&input[..split]);
                let mut output = first_output.into_owned();
                let (second_output, second_events) = chunked.process(&input[split..]);
                output.extend_from_slice(&second_output);
                events.extend(second_events);

                assert_eq!(output, expected_output.as_ref(), "split at {split}");
                assert_eq!(events, expected_events, "split at {split}");
            }
        }
    }

    #[test]
    fn passthrough_normal_text() {
        let mut interceptor = OscInterceptor::new();
        let (output, events) = process_str(&mut interceptor, "hello world");
        assert_eq!(output, "hello world");
        assert!(events.is_empty());
    }

    #[test]
    fn passthrough_unknown_osc() {
        let mut interceptor = OscInterceptor::new();
        // OSC 0 (title) should be passed through
        let (output, events) = process_str(&mut interceptor, "\x1b]0;My Title\x07");
        assert_eq!(output, "\x1b]0;My Title\x07");
        assert!(events.is_empty());
    }

    #[test]
    fn passthrough_osc_8_hyperlinks() {
        let mut interceptor = OscInterceptor::new();
        // OSC 8 hyperlinks must reach the alacritty parser so cells pick up
        // their hyperlink metadata. The interceptor re-emits passthrough OSC
        // with a BEL terminator, which alacritty treats identically to ST.
        let input = "\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\";
        let (output, events) = process_str(&mut interceptor, input);
        assert_eq!(output, "\x1b]8;;https://example.com\x07link\x1b]8;;\x07");
        assert!(events.is_empty());
    }

    #[test]
    fn parse_osc_7_working_directory() {
        let mut interceptor = OscInterceptor::new();
        let (output, events) =
            process_str(&mut interceptor, "\x1b]7;file://localhost/home/user\x07");
        assert!(output.is_empty());
        assert_eq!(
            events,
            vec![OscEvent::WorkingDirectory("/home/user".to_string())]
        );
    }

    #[test]
    fn parse_osc_7_decodes_percent_escaped_paths() {
        let mut interceptor = OscInterceptor::new();
        let (output, events) = process_str(&mut interceptor, "\x1b]7;file://host/tmp/a%20b\x1b\\");

        assert!(output.is_empty());
        assert_eq!(
            events,
            vec![OscEvent::WorkingDirectory("/tmp/a b".to_string())]
        );
    }

    #[test]
    fn parse_osc_9_9_keeps_literal_percent_escapes() {
        let mut interceptor = OscInterceptor::new();
        let (_, events) = process_str(&mut interceptor, "]9;9;\"/c/a%41\"");
        assert_eq!(
            events,
            vec![
                OscEvent::WorkingDirectory("/c/a%41".to_string()),
                OscEvent::PromptIdle,
            ]
        );
    }

    #[test]
    fn parse_osc_9_9_working_directory() {
        let mut interceptor = OscInterceptor::new();
        let (output, events) = process_str(
            &mut interceptor,
            "\x1b]9;9;\\\\wsl.localhost\\Ubuntu\\home\\kamr\x07",
        );
        assert!(output.is_empty());
        assert_eq!(
            events,
            vec![
                OscEvent::WorkingDirectory("\\\\wsl.localhost\\Ubuntu\\home\\kamr".to_string()),
                OscEvent::PromptIdle,
            ]
        );
    }

    #[test]
    fn parse_osc_9_9_working_directory_strips_quotes() {
        let mut interceptor = OscInterceptor::new();
        let (_, events) = process_str(&mut interceptor, "\x1b]9;9;\"C:\\Users\\kamr\"\x07");
        assert_eq!(
            events,
            vec![
                OscEvent::WorkingDirectory("C:\\Users\\kamr".to_string()),
                OscEvent::PromptIdle,
            ]
        );
    }

    #[test]
    fn osc_9_9_with_st_terminator_also_signals_prompt_idle() {
        let mut interceptor = OscInterceptor::new();
        // ConPTY 实际用 ESC \ 结尾
        let (output, events) = process_str(&mut interceptor, "\x1b]9;9;\"C:\\x\"\x1b\\");
        assert!(output.is_empty());
        assert_eq!(events.last(), Some(&OscEvent::PromptIdle));
    }

    #[test]
    fn osc_7_working_directory_does_not_signal_prompt_idle() {
        let mut interceptor = OscInterceptor::new();
        let (_, events) = process_str(&mut interceptor, "\x1b]7;file://host/tmp\x07");
        assert!(!events.contains(&OscEvent::PromptIdle));
    }

    #[test]
    fn osc_9_4_progress_does_not_signal_prompt_idle() {
        let mut interceptor = OscInterceptor::new();
        let (_, events) = process_str(&mut interceptor, "\x1b]9;4;1;50\x07");
        assert!(!events.contains(&OscEvent::PromptIdle));
    }

    #[test]
    fn parse_osc_9_9_working_directory_ignores_empty_path() {
        let mut interceptor = OscInterceptor::new();
        let (_, events) = process_str(&mut interceptor, "\x1b]9;9;\"\"\x07");
        assert!(events.is_empty());
    }

    #[test]
    fn parse_osc_9_4_progress() {
        let mut interceptor = OscInterceptor::new();

        // Progress 50%
        let (_, events) = process_str(&mut interceptor, "\x1b]9;4;1;50\x07");
        assert_eq!(
            events,
            vec![OscEvent::Progress(ProgressState::InProgress(50))]
        );

        // Indeterminate
        let (_, events) = process_str(&mut interceptor, "\x1b]9;4;3;0\x07");
        assert_eq!(
            events,
            vec![OscEvent::Progress(ProgressState::Indeterminate)]
        );

        // Clear
        let (_, events) = process_str(&mut interceptor, "\x1b]9;4;0;0\x07");
        assert_eq!(events, vec![OscEvent::Progress(ProgressState::Clear)]);

        // Error
        let (_, events) = process_str(&mut interceptor, "\x1b]9;4;2;75\x07");
        assert_eq!(events, vec![OscEvent::Progress(ProgressState::Error(75))]);
    }

    #[test]
    fn parse_osc_133_shell_integration() {
        let mut interceptor = OscInterceptor::new();

        let (_, events) = process_str(&mut interceptor, "\x1b]133;A\x07");
        assert_eq!(events, vec![OscEvent::ShellPromptStart]);

        let (_, events) = process_str(&mut interceptor, "\x1b]133;B\x07");
        assert_eq!(events, vec![OscEvent::ShellCommandStart]);

        let (_, events) = process_str(&mut interceptor, "\x1b]133;C\x07");
        assert_eq!(events, vec![OscEvent::ShellCommandExecuting]);

        let (_, events) = process_str(&mut interceptor, "\x1b]133;D;0\x07");
        assert_eq!(events, vec![OscEvent::ShellCommandFinished(Some(0))]);

        let (_, events) = process_str(&mut interceptor, "\x1b]133;D;1\x07");
        assert_eq!(events, vec![OscEvent::ShellCommandFinished(Some(1))]);

        // D without exit code
        let (_, events) = process_str(&mut interceptor, "\x1b]133;D\x07");
        assert_eq!(events, vec![OscEvent::ShellCommandFinished(None)]);
    }

    #[test]
    fn parse_osc_with_st_terminator() {
        let mut interceptor = OscInterceptor::new();
        // ST = ESC \ instead of BEL
        let (output, events) = process_str(&mut interceptor, "\x1b]9;4;1;50\x1b\\");
        assert!(output.is_empty());
        assert_eq!(
            events,
            vec![OscEvent::Progress(ProgressState::InProgress(50))]
        );
    }

    #[test]
    fn notification_sequences_are_ignored() {
        let mut interceptor = OscInterceptor::new();

        let (output, events) = process_str(&mut interceptor, "\x1b]9;Build complete!\x07");
        assert!(output.is_empty());
        assert!(events.is_empty());

        let (output, events) = process_str(
            &mut interceptor,
            "\x1b]777;notify;Build;Completed successfully\x07",
        );
        assert!(output.is_empty());
        assert!(events.is_empty());
    }

    #[test]
    fn multiple_sequences_in_stream() {
        let mut interceptor = OscInterceptor::new();
        let input = "prefix\x1b]133;A\x07middle\x1b]9;4;1;50\x07suffix";
        let (output, events) = process_str(&mut interceptor, input);
        assert_eq!(output, "prefixmiddlesuffix");
        assert_eq!(
            events,
            vec![
                OscEvent::ShellPromptStart,
                OscEvent::Progress(ProgressState::InProgress(50))
            ]
        );
    }

    #[test]
    fn interleaved_with_other_escapes() {
        let mut interceptor = OscInterceptor::new();
        // Mix of cursor movement (CSI) and our OSC
        let input = "\x1b[Hstart\x1b]133;C\x07\x1b[Jend";
        let (output, events) = process_str(&mut interceptor, input);
        // CSI sequences should pass through
        assert_eq!(output, "\x1b[Hstart\x1b[Jend");
        assert_eq!(events, vec![OscEvent::ShellCommandExecuting]);
    }

    #[test]
    fn incremental_processing() {
        let mut interceptor = OscInterceptor::new();

        // Send partial OSC
        let (output1, events1) = process_str(&mut interceptor, "\x1b]133;");
        assert!(output1.is_empty());
        assert!(events1.is_empty());

        // Complete the OSC
        let (output2, events2) = process_str(&mut interceptor, "A\x07rest");
        assert_eq!(output2, "rest");
        assert_eq!(events2, vec![OscEvent::ShellPromptStart]);
    }
}
