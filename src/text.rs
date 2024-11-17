// This module provides a way to render styled text, either as HTML or using terminal
// escape codes. It deliberately resembles ratatui's text rendering system
// (https://docs.rs/ratatui/latest/ratatui/text/struct.Text.html) so
// that if ratatui gains support for hyperlinks
// (https://github.com/ratatui/ratatui/issues/1028) maybe I can easily port this
// code to use that?
//
// I discovered that I absolutely hate terminal hacking, I don't really know
// if that's a reason not to contribute to ratatui. Anyway, the real reason I'm
// not contributing to ratatui is that I wanna race to finish this project
// instead of getting bogged down in side-quests!

use std::{
    borrow::Cow,
    fmt::{self, Display, Formatter},
};

use colored::{ColoredString, Colorize as _};

// Represents a block of text, potentially with styling. Note this always
// represents a block, you can't represent a string without a newline at the
// end (I'm not sure if that's the same in Ratatui).
pub struct Text<'a> {
    pub lines: Vec<Line<'a>>,
}

impl<'a, T> FromIterator<T> for Text<'a>
where
    T: Into<Line<'a>>,
{
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self {
            lines: iter.into_iter().map(|i| i.into()).collect(),
        }
    }
}

// Shorthand for constructing a Text with a single Line.
impl<'a, T: Into<Line<'a>>> From<T> for Text<'a> {
    fn from(t: T) -> Self {
        Self {
            lines: vec![t.into()],
        }
    }
}
impl<'a> Text<'a> {
    // Render the text with style applied using ANSI commands. Use Display on the returned value
    // to write it out.
    pub fn ansi(&self) -> RenderAnsi {
        RenderAnsi { text: self }
    }

    // Render to an HTML <pre> element.
    pub fn html_pre(&self) -> RenderHtmlPre {
        RenderHtmlPre { text: self }
    }
}

pub struct RenderAnsi<'a> {
    text: &'a Text<'a>,
}

impl<'a> Display for RenderAnsi<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for line in self.text.lines.iter() {
            writeln!(f, "{}", RenderAnsiLine { line })?;
        }
        Ok(())
    }
}

pub struct RenderHtmlPre<'a> {
    text: &'a Text<'a>,
}

impl<'a> Display for RenderHtmlPre<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "<pre>")?;
        for line in self.text.lines.iter() {
            writeln!(f, "{}", RenderHtmlLine { line })?;
        }
        writeln!(f, "</pre>")
    }
}

pub struct Line<'a> {
    pub spans: Vec<Span<'a>>,
}

impl<'a, T> FromIterator<T> for Line<'a>
where
    T: Into<Span<'a>>,
{
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self {
            spans: Vec::from_iter(iter.into_iter().map(|i| i.into())),
        }
    }
}

struct RenderAnsiLine<'a> {
    line: &'a Line<'a>,
}

impl<'a> Display for RenderAnsiLine<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for span in self.line.spans.iter() {
            write!(f, "{}", RenderAnsiSpan { span })?;
        }
        Ok(())
    }
}

// TODO: Deduplicate with generics?
struct RenderHtmlLine<'a> {
    line: &'a Line<'a>,
}

impl<'a> Display for RenderHtmlLine<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for span in self.line.spans.iter() {
            write!(f, "{}", RenderHtmlSpan { span })?;
        }
        Ok(())
    }
}

// Shorthand for constructing a Line with a single Span.
impl<'a, T: Into<Span<'a>>> From<T> for Line<'a> {
    fn from(t: T) -> Self {
        Self {
            spans: vec![t.into()],
        }
    }
}

pub struct Span<'a> {
    pub style: Style<'a>,
    // The cow is copied from Ratatui. My understanding is that this is there to
    // be generic across ownership or reference.
    pub content: Cow<'a, str>,
}

impl<'a, T: Into<Cow<'a, str>>> From<T> for Span<'a> {
    fn from(t: T) -> Span<'a> {
        Span::raw(t)
    }
}

impl<'a> Span<'a> {
    pub fn raw(content: impl Into<Cow<'a, str>>) -> Self {
        Self {
            content: content.into(),
            style: Style::default(),
        }
    }

    pub fn styled(content: impl Into<Cow<'a, str>>, style: Style<'a>) -> Self {
        Self {
            content: content.into(),
            style,
        }
    }
}

struct RenderAnsiSpan<'a> {
    span: &'a Span<'a>,
}

impl<'a> Display for RenderAnsiSpan<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let output = self.span.content.as_ref();
        let mut output = match self.span.style.bg {
            // TODO: ColoredString is not very useful here any more.
            None => ColoredString::from(output),
            Some(Color::Red) => output.on_red(),
            Some(Color::Green) => output.on_green(),
            Some(Color::BrightRed) => output.on_bright_red(),
        };
        if self.span.style.bold {
            output = output.bold();
        }
        // Renders a hyperlink like in
        // https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda.
        if let Some(ref url) = &self.span.style.hyperlink {
            write!(f, "\u{1b}]8;;{}\u{1b}\\{}\u{1b}]8;;\u{1b}\\", url, output)
        } else {
            write!(f, "{}", &output.to_string())
        }
    }
}

struct RenderHtmlSpan<'a> {
    span: &'a Span<'a>,
}

impl<'a> Display for RenderHtmlSpan<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let bg = match self.span.style.bg {
            None => "inherit",
            Some(Color::Red) => "maroon",
            Some(Color::Green) => "green",
            Some(Color::BrightRed) => "red",
        };
        let weight = if self.span.style.bold {
            "bold"
        } else {
            "normal"
        };
        write!(
            f,
            r#"<span style="background-color: {}; font-weight: {}">{}</span>"#,
            bg,
            weight,
            self.span.content.as_ref()
        )
    }
}

pub enum Color {
    Red,
    Green,
    BrightRed,
}

#[derive(Default)]
pub struct Style<'a> {
    pub bg: Option<Color>,
    pub bold: bool,
    pub hyperlink: Option<Cow<'a, str>>,
}

impl<'a> Style<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_red(mut self) -> Self {
        self.bg = Some(Color::Red);
        self
    }

    pub fn on_bright_red(mut self) -> Self {
        self.bg = Some(Color::BrightRed);
        self
    }

    pub fn on_green(mut self) -> Self {
        self.bg = Some(Color::Green);
        self
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
}
