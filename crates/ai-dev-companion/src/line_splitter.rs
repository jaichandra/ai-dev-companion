//! Splits a stream of text chunks into lines (port of `core/line-splitter.js`).
//! Used to hand a child's stdout to a listener line by line while the caller
//! still buffers the whole output. A listener's failure must never break the
//! run it is observing: whatever the listener returns is ignored (a `Result`
//! error is swallowed).

pub struct LineSplitter<F> {
    pending: String,
    on_line: F,
}

impl<F, R> LineSplitter<F>
where
    F: FnMut(&str) -> R,
{
    pub fn new(on_line: F) -> Self {
        LineSplitter { pending: String::new(), on_line }
    }

    fn emit(&mut self, line: &str) {
        if !line.is_empty() {
            let _ = (self.on_line)(line);
        }
    }

    pub fn write(&mut self, chunk: &str) {
        self.pending.push_str(chunk);
        while let Some(i) = self.pending.find('\n') {
            let rest = self.pending.split_off(i + 1);
            let mut line = std::mem::replace(&mut self.pending, rest);
            line.pop(); // the \n
            if line.ends_with('\r') {
                line.pop();
            }
            self.emit(&line);
        }
    }

    pub fn flush(&mut self) {
        let mut rest = std::mem::take(&mut self.pending);
        if rest.ends_with('\r') {
            rest.pop();
        }
        self.emit(&rest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_chunks_on_newlines_and_joins_a_line_split_across_chunks() {
        let mut lines: Vec<String> = Vec::new();
        {
            let mut s = LineSplitter::new(|l: &str| lines.push(l.to_string()));
            s.write("{\"a\":1}\n{\"b\"");
            s.write(":2}\n\r\n{\"c\":3}");
            s.flush();
        }
        assert_eq!(lines, ["{\"a\":1}", "{\"b\":2}", "{\"c\":3}"]);
    }

    #[test]
    fn the_last_line_waits_for_flush() {
        let mut lines: Vec<String> = Vec::new();
        let mut s = LineSplitter::new(|l: &str| lines.push(l.to_string()));
        s.write("a\nb");
        assert_eq!(s.pending, "b");
        s.flush();
        drop(s);
        assert_eq!(lines, ["a", "b"]);
    }

    #[test]
    fn drops_carriage_returns_ignores_blanks_and_survives_a_failing_listener() {
        let mut lines: Vec<String> = Vec::new();
        {
            let mut s = LineSplitter::new(|l: &str| -> Result<(), String> {
                lines.push(l.to_string());
                Err("boom".into())
            });
            s.write("one\r\n\ntwo\n");
            s.flush();
        }
        assert_eq!(lines, ["one", "two"]);
    }

    #[test]
    fn multibyte_text_is_not_split_mid_character() {
        let mut lines: Vec<String> = Vec::new();
        let mut s = LineSplitter::new(|l: &str| lines.push(l.to_string()));
        s.write("h\u{e9}llo \u{1F600}\nw\u{f6}rld\n");
        drop(s);
        assert_eq!(lines, ["h\u{e9}llo \u{1F600}", "w\u{f6}rld"]);
    }
}
