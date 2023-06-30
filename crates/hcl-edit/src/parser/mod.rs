//! An HCL parser which keeps track of whitespace, comments and span information.

mod context;
mod error;
mod expr;
mod number;
mod repr;
mod state;
mod string;
mod structure;
mod template;
#[cfg(test)]
mod tests;
mod trivia;

pub use self::error::{Error, Location};
use self::{error::ParseError, expr::expr, structure::body, template::template};
use crate::{expr::Expression, structure::Body, template::Template};
use winnow::{combinator::eof, combinator::terminated, stream::AsBytes, stream::Located, Parser};

type Input<'a> = Located<&'a [u8]>;

type PResult<'a, O, E = ParseError<Input<'a>>> = winnow::PResult<O, E>;

/// Parse an input into a [`Body`](crate::structure::Body).
///
/// # Errors
///
/// Returns an error if the input does not resemble a valid HCL body.
pub fn parse_body(input: &str) -> Result<Body, Error> {
    let mut body = parse_complete(input, body)?;
    body.despan(input);
    Ok(body)
}

/// Parse an input into an [`Expression`](crate::expr::Expression).
///
/// # Errors
///
/// Returns an error if the input does not resemble a valid HCL expression.
pub fn parse_expr(input: &str) -> Result<Expression, Error> {
    let mut expr = parse_complete(input, expr)?;
    expr.despan(input);
    Ok(expr)
}

/// Parse an input into a [`Template`](crate::template::Template).
///
/// # Errors
///
/// Returns an error if the input does not resemble a valid HCL template.
pub fn parse_template(input: &str) -> Result<Template, Error> {
    let mut template = parse_complete(input, template)?;
    template.despan(input);
    Ok(template)
}

fn parse_complete<'a, P, O>(input: &'a str, parser: P) -> Result<O, Error>
where
    P: Parser<Input<'a>, O, ParseError<Input<'a>>>,
{
    let mut stream = Input::new(input.as_bytes());

    terminated(parser, eof)
        .parse_next(&mut stream)
        .map_err(|err| {
            Error::from_parse_error(
                input.as_bytes(),
                stream.as_bytes(),
                &err.into_inner().expect("`Incomplete` isn't used"),
            )
        })
}
