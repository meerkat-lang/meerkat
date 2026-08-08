//! HTML values as an abstract data type
//!
//! Outside this module, no code knows how an HTML value is represented.
//! Today it is backed by a single rendered string, but that is private:
//! callers construct an `Html` through [`Html::from_rendered`] and read it
//! back through [`Html::as_str`] or `Display`. This boundary lets the
//! representation change later (for example, a structured tree that can be
//! validated to catch errors earlier and mitigate injection) without
//! affecting any code outside this module.

use std::fmt;

use crate::runtime::ast::{Expr, Value};

/// An evaluated HTML value
///
/// The internal representation is deliberately private. See the module
/// documentation for the rationale behind treating HTML as an abstract
/// data type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Html {
    /// The rendered markup
    ///
    /// Private on purpose: no code outside this module may depend on HTML
    /// being represented as a string.
    rendered: String,
    /// #153: event handlers bound to elements, as (event name, action value)
    /// pairs. These are not part of the rendered string; the client attaches
    /// them as real listeners. Empty for HTML with no handlers.
    handlers: Vec<(String, Value)>,
}

impl Html {
    /// Construct an `Html` value from already-rendered markup
    ///
    /// Args:
    ///     `rendered` (`String`): The rendered markup text
    ///
    /// Returns:
    ///     `Html`: The constructed HTML value
    pub fn from_rendered(rendered: String) -> Self {
        Html {
            rendered,
            handlers: Vec::new(),
        }
    }

    /// #153: the event handlers bound in this HTML value, as (event, action)
    /// pairs. The client reads these to attach real event listeners.
    pub fn handlers(&self) -> &[(String, Value)] {
        &self.handlers
    }

    /// Borrow the rendered markup as a string slice
    ///
    /// This is the read accessor used by the printer and the network codec.
    /// It exposes the rendered content without revealing that the value is
    /// stored as a string.
    ///
    /// Returns:
    ///     `&str`: The rendered markup
    pub fn as_str(&self) -> &str {
        &self.rendered
    }
}

/// Implement the `Display` trait for the `Html` type
///
/// Prints the rendered markup.
impl fmt::Display for Html {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.rendered)
    }
}

/// A single part of an HTML template: literal markup text or an embedded
/// expression to be evaluated and rendered in place.
///
/// This type is internal to the html module. Outside code never sees that an
/// HTML template is a sequence of text and expressions; it works through
/// [`HtmlTemplate`]'s interface. Keeping this private is what allows a future
/// structured, validatable representation to replace it (validation applies to
/// HTML expressions, not values) without affecting other code.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum HtmlPart {
    Text(String),
    Expr(Box<Expr>),
    /// #153: an event handler bound to an `on*` attribute. `event` is the
    /// attribute name (e.g. "onclick"); `expr` evaluates to an action value.
    Handler {
        event: String,
        expr: Box<Expr>,
    },
}

/// The representation of an HTML template expression, before evaluation.
///
/// The internal structure (a list of text and embedded expressions) is
/// private. Callers construct a template by parsing, reach the embedded
/// expressions through [`HtmlTemplate::embedded_exprs`] /
/// [`HtmlTemplate::embedded_exprs_mut`] (for dependency analysis and alpha
/// renaming), and render it through [`HtmlTemplate::render`]. This mirrors the
/// [`Html`] value ADT and is the boundary at which HTML validation will later
/// be added.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HtmlTemplate {
    parts: Vec<HtmlPart>,
}

impl HtmlTemplate {
    /// Iterate the embedded expressions of this template, in order.
    ///
    /// Used by dependency analysis and alpha renaming so those passes can
    /// operate on the interpolated expressions without knowing how the
    /// template is represented.
    pub fn embedded_exprs(&self) -> impl Iterator<Item = &Expr> {
        self.parts.iter().filter_map(|p| match p {
            HtmlPart::Text(_) => None,
            HtmlPart::Expr(e) => Some(e.as_ref()),
            HtmlPart::Handler { expr, .. } => Some(expr.as_ref()),
        })
    }

    /// Mutable counterpart to [`HtmlTemplate::embedded_exprs`].
    pub fn embedded_exprs_mut(&mut self) -> impl Iterator<Item = &mut Expr> {
        self.parts.iter_mut().filter_map(|p| match p {
            HtmlPart::Text(_) => None,
            HtmlPart::Expr(e) => Some(e.as_mut()),
            HtmlPart::Handler { expr, .. } => Some(expr.as_mut()),
        })
    }

    /// #39: Print this template's AST structure via the given printer.
    ///
    /// The html module owns how a template is displayed, so the internal
    /// representation stays hidden even from the printer. Literal text is
    /// printed directly; embedded expressions are delegated back to the
    /// printer's `print_expr`.
    pub fn print_ast(&self, printer: &crate::runtime::ast::printer::AstPrinter, indent: usize) {
        for part in &self.parts {
            match part {
                HtmlPart::Text(t) => {
                    printer.print_indent(indent);
                    println!("Text: {:?}", t);
                }
                HtmlPart::Expr(e) => printer.print_expr(e, indent),
                HtmlPart::Handler { event, expr } => {
                    printer.print_indent(indent);
                    println!("Handler({}):", event);
                    printer.print_expr(expr, indent + 1);
                }
            }
        }
    }

    /// Render the template into an [`Html`] value.
    ///
    /// The caller supplies the already-evaluated value for each embedded
    /// expression, in the same order as [`HtmlTemplate::embedded_exprs`]. The
    /// evaluator owns evaluation (it has the async context); this method owns
    /// assembling the final markup, so the string representation stays inside
    /// this module. Literal text is copied verbatim; each embedded value is
    /// formatted via its `Display`.
    ///
    /// Returns `None` if `values` has fewer entries than there are embedded
    /// expressions (a caller bug).
    pub fn render(&self, values: &[Value]) -> Option<Html> {
        let mut rendered = String::new();
        let mut handlers = Vec::new();
        let mut next = 0usize;
        for part in &self.parts {
            match part {
                HtmlPart::Text(t) => rendered.push_str(t),
                HtmlPart::Expr(_) => {
                    let v = values.get(next)?;
                    // #39: interpolate using the value's plain display. String
                    // values currently include surrounding quotes; refining
                    // string interpolation formatting is a known follow-up.
                    use std::fmt::Write as _;
                    let _ = write!(rendered, "{}", v);
                    next += 1;
                }
                HtmlPart::Handler { event, .. } => {
                    // #153: a handler's value is the action to run on the
                    // event; it is captured as a handler rather than rendered
                    // into the markup.
                    let v = values.get(next)?;
                    handlers.push((event.clone(), v.clone()));
                    next += 1;
                }
            }
        }
        Some(Html { rendered, handlers })
    }
}

/// Builder for [`HtmlTemplate`].
///
/// The parser constructs a template through this builder, appending literal
/// text and embedded expressions, without naming the internal `HtmlPart`
/// representation. This keeps the template representation encapsulated in this
/// module while interning stays in `runtime::parser` (the interner is
/// append-only and only `parser`/`net::codec` may write to it).
#[derive(Debug, Default)]
pub struct HtmlTemplateBuilder {
    parts: Vec<HtmlPart>,
}

impl HtmlTemplateBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        HtmlTemplateBuilder { parts: Vec::new() }
    }

    /// Append literal markup text (skips empty strings).
    pub fn push_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.parts.push(HtmlPart::Text(text.to_string()));
        }
    }

    /// Append an embedded (interpolated) expression.
    pub fn push_expr(&mut self, expr: Expr) {
        self.parts.push(HtmlPart::Expr(Box::new(expr)));
    }

    /// #153: append an event handler bound to an `on*` attribute.
    pub fn push_handler(&mut self, event: String, expr: Expr) {
        self.parts.push(HtmlPart::Handler {
            event,
            expr: Box::new(expr),
        });
    }

    /// Finish building the template.
    pub fn build(self) -> HtmlTemplate {
        HtmlTemplate { parts: self.parts }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that an `Html` value round-trips its rendered content through
    /// the public accessor and `Display`.
    #[test]
    fn test_html_render_roundtrip() {
        let html = Html::from_rendered("<p>The count is 0.</p>".to_string());
        assert_eq!(html.as_str(), "<p>The count is 0.</p>");
        assert_eq!(html.to_string(), "<p>The count is 0.</p>");
    }

    /// Verify that equality is by rendered content.
    #[test]
    fn test_html_equality() {
        let a = Html::from_rendered("<b>x</b>".to_string());
        let b = Html::from_rendered("<b>x</b>".to_string());
        let c = Html::from_rendered("<b>y</b>".to_string());
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
