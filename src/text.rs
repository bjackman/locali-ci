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

use std::{borrow::Cow, fmt};

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
    // Render the text with style applied using ANSI commands.
    pub fn render_ansi(&self, w: &mut impl fmt::Write) -> fmt::Result {
        for line in self.lines.iter() {
            line.render_ansi(w)?;
            w.write_str("\n")?;
        }
        Ok(())
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

impl<'a> Line<'a> {
    fn render_ansi(&self, w: &mut impl fmt::Write) -> fmt::Result {
        for span in self.spans.iter() {
            span.render_ansi(w)?
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

    fn render_ansi(&self, w: &mut impl fmt::Write) -> fmt::Result {
        let output = self.content.as_ref();
        let mut output = match self.style.bg {
            // TODO: ColoredString is not very useful here any more.
            None => ColoredString::from(output),
            Some(Color::Red) => output.on_red(),
            Some(Color::Green) => output.on_green(),
            Some(Color::Blue) => output.on_blue(),
            Some(Color::BrightRed) => output.on_bright_red(),
        };
        if self.style.bold {
            output = output.bold();
        }
        // Renders a hyperlink like in
        // https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda.
        if let Some(url) = &self.style.hyperlink {
            w.write_fmt(format_args!(
                "\u{1b}]8;;{}\u{1b}\\{}\u{1b}]8;;\u{1b}\\",
                url, output
            ))
        } else {
            w.write_str(&output.to_string())
        }
    }
}

pub enum Color {
    Red,
    Green,
    #[expect(dead_code)]
    Blue,
    #[expect(dead_code)]
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

    pub fn on_green(mut self) -> Self {
        self.bg = Some(Color::Green);
        self
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
}
