use super::{
    context::{cut_char, cut_ident, cut_tag, Context, Expected},
    error::ParseError,
    number::number as num,
    repr::{decorated, prefix_decorated, spanned, suffix_decorated},
    state::ExprParseState,
    string::{from_utf8_unchecked, ident, is_id_start, raw_string, str_ident, string},
    template::{heredoc_template, string_template},
    trivia::{line_comment, sp, ws},
    IResult, Input,
};
use crate::{
    expr::{
        Array, BinaryOperator, Expression, ForCond, ForExpr, ForIntro, FuncArgs, FuncCall, Null,
        Object, ObjectKey, ObjectValue, ObjectValueAssignment, ObjectValueTerminator, Parenthesis,
        Splat, TraversalOperator, UnaryOperator,
    },
    template::HeredocTemplate,
    Decorate, Decorated, Formatted, Ident, RawString, SetSpan, Spanned,
};
use std::cell::RefCell;
use winnow::{
    ascii::{crlf, dec_uint, line_ending, newline, space0},
    combinator::{
        alt, cut_err, delimited, fail, not, opt, peek, preceded, repeat, separated0, separated1,
        separated_pair, success, terminated,
    },
    dispatch,
    token::{any, none_of, one_of, take},
    Parser,
};

pub(super) fn expr(input: Input) -> IResult<Input, Expression> {
    let state = RefCell::new(ExprParseState::default());
    let (input, _) = expr_inner(&state).parse_next(input)?;
    let expr = state.into_inner().into_expr();
    Ok((input, expr))
}

fn expr_inner<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        let (mut input, span) = expr_term(state).span().parse_next(input)?;
        state.borrow_mut().on_span(span);

        loop {
            // Parse the next whitespace sequence and only add it as decor suffix to the expression if
            // we actually encounter a traversal, conditional or binary operation. We'll rewind the
            // parser if none of these follow.
            let (remaining_input, suffix) = sp.span().parse_next(input)?;

            // This is essentially a `peek` for the next two bytes to identify the following operation.
            if let Ok((_, peek)) = take::<_, _, ParseError<_>>(2usize).parse_next(remaining_input) {
                match peek {
                    // The sequence `..` might introduce a `...` operator within a for object expr
                    // or after the last argument of a function call, do not mistakenly parse it as
                    // a `.` traversal operator.
                    //
                    // `//` and `/*` are comment starts. Do not mistakenly parse a `/` as binary
                    // division operator.
                    b"//" | b"/*" | b".." => return Ok((input, ())),
                    // Traversal operator.
                    //
                    // Note: after the traversal is consumed, the loop is entered again to consume
                    // a potentially following conditional or binary operation.
                    [b'.' | b'[', _] => {
                        state.borrow_mut().on_ws(suffix);
                        (input, _) = traversal(state).parse_next(remaining_input)?;
                        continue;
                    }
                    // Conditional.
                    [b'?', _] => {
                        state.borrow_mut().on_ws(suffix);
                        return conditional(state).parse_next(remaining_input);
                    }
                    // Binary operation.
                    //
                    // Note: matching a single `=` is ambiguous as it could also be an object
                    // key-value separator, so we'll need to match on `==`.
                    b"=="
                    | [b'!' | b'<' | b'>' | b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|', _] => {
                        state.borrow_mut().on_ws(suffix);
                        return binary_op(state).parse_next(remaining_input);
                    }
                    // None of the above matched.
                    _ => return Ok((input, ())),
                }
            }

            // We hit the end of input.
            return Ok((input, ()));
        }
    }
}

fn expr_term<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        dispatch! {peek(any);
            b'"' => stringlike(state),
            b'[' => array(state),
            b'{' => object(state),
            b'0'..=b'9' => number(state),
            b'<' => heredoc(state),
            b'-' => alt((neg_number(state), unary_op(state))),
            b'!' => unary_op(state),
            b'(' => parenthesis(state),
            b if is_id_start(b) => identlike(state),
            _ => cut_err(fail)
                .context(Context::Expression("expression"))
                .context(Context::Expected(Expected::Char('"')))
                .context(Context::Expected(Expected::Char('[')))
                .context(Context::Expected(Expected::Char('{')))
                .context(Context::Expected(Expected::Char('-')))
                .context(Context::Expected(Expected::Char('!')))
                .context(Context::Expected(Expected::Char('(')))
                .context(Context::Expected(Expected::Char('_')))
                .context(Context::Expected(Expected::Char('<')))
                .context(Context::Expected(Expected::Description("letter")))
                .context(Context::Expected(Expected::Description("digit"))),
        }
        .parse_next(input)
    }
}

fn stringlike<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        alt((
            string.map(|string| {
                state
                    .borrow_mut()
                    .on_expr_term(Expression::String(Decorated::new(string)));
            }),
            string_template.map(|template| {
                state
                    .borrow_mut()
                    .on_expr_term(Expression::Template(template));
            }),
        ))
        .parse_next(input)
    }
}

fn number<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        num.with_recognized()
            .map(|(num, repr)| {
                let mut num = Formatted::new(num);
                num.set_repr(unsafe { from_utf8_unchecked(repr, "`num` filters out non-ascii") });
                state.borrow_mut().on_expr_term(Expression::Number(num));
            })
            .parse_next(input)
    }
}

fn neg_number<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        preceded((b'-', sp), num)
            .with_recognized()
            .try_map(|(num, repr)| {
                std::str::from_utf8(repr).map(|repr| {
                    let mut num = Formatted::new(-num);
                    num.set_repr(repr);
                    state.borrow_mut().on_expr_term(Expression::Number(num));
                })
            })
            .parse_next(input)
    }
}

fn traversal<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        repeat(
            1..,
            prefix_decorated(sp, traversal_operator.map(Decorated::new)),
        )
        .map(|operators| state.borrow_mut().on_traversal(operators))
        .parse_next(input)
    }
}

fn traversal_operator(input: Input) -> IResult<Input, TraversalOperator> {
    dispatch! {any;
        b'.' => prefix_decorated(
            ws,
            dispatch! {peek(any);
                b'*' => b'*'.value(TraversalOperator::AttrSplat(Decorated::new(Splat))),
                b'0'..=b'9' => dec_uint.map(|index: u64| TraversalOperator::LegacyIndex(index.into())),
                b if is_id_start(b) => ident.map(TraversalOperator::GetAttr),
                _ => cut_err(fail)
                    .context(Context::Expression("traversal operator"))
                    .context(Context::Expected(Expected::Char('*')))
                    .context(Context::Expected(Expected::Description("identifier")))
                    .context(Context::Expected(Expected::Description("unsigned integer"))),
            },
        ),
        b'[' => terminated(
            decorated(
                ws,
                dispatch! {peek(any);
                    b'*' => b'*'.value(TraversalOperator::FullSplat(Decorated::new(Splat))),
                    _ => expr.map(TraversalOperator::Index),
                },
                ws,
            ),
            cut_char(']'),
        ),
        _ => fail,
    }
    .parse_next(input)
}

fn unary_op<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        preceded(
            (spanned(unary_operator.map(Spanned::new)), sp.span())
                .map(|(operator, span)| state.borrow_mut().on_unary_op(operator, span)),
            expr_term(state),
        )
        .void()
        .parse_next(input)
    }
}

fn unary_operator(input: Input) -> IResult<Input, UnaryOperator> {
    dispatch! {any;
        b'-' => success(UnaryOperator::Neg),
        b'!' => success(UnaryOperator::Not),
        _ => fail,
    }
    .parse_next(input)
}

fn binary_op<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        (
            spanned(binary_operator.map(Spanned::new)),
            prefix_decorated(sp, expr),
        )
            .map(|(operator, rhs_expr)| state.borrow_mut().on_binary_op(operator, rhs_expr))
            .parse_next(input)
    }
}

fn binary_operator(input: Input) -> IResult<Input, BinaryOperator> {
    dispatch! {any;
        b'=' => b'='.value(BinaryOperator::Eq),
        b'!' => b'='.value(BinaryOperator::NotEq),
        b'<' => alt((
            b'='.value(BinaryOperator::LessEq),
            success(BinaryOperator::Less),
        )),
        b'>' => alt((
            b'='.value(BinaryOperator::GreaterEq),
            success(BinaryOperator::Greater),
        )),
        b'+' => success(BinaryOperator::Plus),
        b'-' => success(BinaryOperator::Minus),
        b'*' => success(BinaryOperator::Mul),
        b'/' => success(BinaryOperator::Div),
        b'%' => success(BinaryOperator::Mod),
        b'&' => b'&'.value(BinaryOperator::And),
        b'|' => b'|'.value(BinaryOperator::Or),
        _ => fail,
    }
    .parse_next(input)
}

fn conditional<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        (
            preceded(b'?', decorated(sp, expr, sp)),
            preceded(cut_char(':'), prefix_decorated(sp, expr)),
        )
            .map(|(true_expr, false_expr)| state.borrow_mut().on_conditional(true_expr, false_expr))
            .parse_next(input)
    }
}

fn array<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        delimited(
            b'[',
            for_expr_or_items(for_list_expr(state), array_items(state)),
            cut_char(']'),
        )
        .parse_next(input)
    }
}

fn for_list_expr<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        (for_intro, decorated(ws, expr, ws), opt(for_cond))
            .map(|(intro, value_expr, cond)| {
                let mut expr = ForExpr::new(intro, value_expr);
                expr.cond = cond;

                state
                    .borrow_mut()
                    .on_expr_term(Expression::ForExpr(Box::new(expr)));
            })
            .parse_next(input)
    }
}

fn array_items<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        let values = separated0(decorated(ws, preceded(not(b']'), expr), ws), b',');

        (values, opt(b','), raw_string(ws))
            .map(|(values, comma, trailing)| {
                let values: Vec<_> = values;
                let mut array = Array::from(values);
                if comma.is_some() {
                    array.set_trailing_comma(true);
                }
                array.set_trailing(trailing);
                state.borrow_mut().on_expr_term(Expression::Array(array));
            })
            .parse_next(input)
    }
}

fn object<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        delimited(
            b'{',
            for_expr_or_items(for_object_expr(state), object_items(state)),
            cut_char('}'),
        )
        .parse_next(input)
    }
}

fn for_object_expr<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        (
            for_intro,
            separated_pair(
                decorated(ws, expr, ws),
                cut_tag("=>"),
                decorated(ws, expr, ws),
            ),
            opt(b"..."),
            opt(for_cond),
        )
            .map(|(intro, (key_expr, value_expr), grouping, cond)| {
                let mut expr = ForExpr::new(intro, value_expr);
                expr.key_expr = Some(key_expr);
                expr.grouping = grouping.is_some();
                expr.cond = cond;

                state
                    .borrow_mut()
                    .on_expr_term(Expression::ForExpr(Box::new(expr)));
            })
            .parse_next(input)
    }
}

fn object_items<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        let mut remaining_input = input;
        let mut object = Object::new();

        loop {
            let (input, trailing) = raw_string(ws).parse_next(remaining_input)?;
            let (input, ch) = peek(any).parse_next(input)?;

            if ch == b'}' {
                object.set_trailing(trailing);
                state.borrow_mut().on_expr_term(Expression::Object(object));
                return Ok((input, ()));
            }

            let (input, mut key) = object_key(input)?;
            let (input, mut value) = object_value(input)?;
            key.decor_mut().set_prefix(trailing);

            // Look for the closing brace and return or consume the object item separator and proceed
            // with the next object item, if any.
            let (input, ch) = peek(any).parse_next(input)?;

            let (input, value_terminator) = match ch {
                b'}' => {
                    value.set_terminator(ObjectValueTerminator::None);
                    object.insert(key, value);
                    state.borrow_mut().on_expr_term(Expression::Object(object));
                    return Ok((input, ()));
                }
                b'\r' => crlf
                    .value(ObjectValueTerminator::Newline)
                    .parse_next(input)?,
                b'\n' => newline
                    .value(ObjectValueTerminator::Newline)
                    .parse_next(input)?,
                b',' => take(1usize)
                    .value(ObjectValueTerminator::Comma)
                    .parse_next(input)?,
                b'#' | b'/' => {
                    let (input, comment_span) = line_comment.span().parse_next(input)?;

                    let decor = value.expr_mut().decor_mut();

                    // Associate the trailing comment with the item value, updating the span if it
                    // already has a decor suffix.
                    let suffix_start = match decor.suffix() {
                        Some(suffix) => suffix.span().unwrap().start,
                        None => comment_span.start,
                    };

                    decor.set_suffix(RawString::from_span(suffix_start..comment_span.end));

                    line_ending
                        .value(ObjectValueTerminator::Newline)
                        .parse_next(input)?
                }
                _ => {
                    return cut_err(fail)
                        .context(Context::Expression("object item"))
                        .context(Context::Expected(Expected::Char('}')))
                        .context(Context::Expected(Expected::Char(',')))
                        .context(Context::Expected(Expected::Char('\n')))
                        .parse_next(input)
                }
            };

            value.set_terminator(value_terminator);
            object.insert(key, value);
            remaining_input = input;
        }
    }
}

fn object_key(input: Input) -> IResult<Input, ObjectKey> {
    suffix_decorated(
        expr.map(|expr| {
            // Variable identifiers without traversal are treated as identifier object keys.
            //
            // Handle this case here by converting the variable into an identifier. This
            // avoids re-parsing the whole key-value pair when an identifier followed by a
            // traversal operator is encountered.
            if let Expression::Variable(variable) = expr {
                ObjectKey::Ident(Decorated::new(variable.into_value()))
            } else {
                ObjectKey::Expression(expr)
            }
        }),
        sp,
    )
    .parse_next(input)
}

fn object_value(input: Input) -> IResult<Input, ObjectValue> {
    (object_value_assignment, decorated(sp, expr, sp))
        .map(|(assignment, expr)| {
            let mut value = ObjectValue::new(expr);
            value.set_assignment(assignment);
            value
        })
        .parse_next(input)
}

fn object_value_assignment(input: Input) -> IResult<Input, ObjectValueAssignment> {
    dispatch! {any;
        b'=' => success(ObjectValueAssignment::Equals),
        b':' => success(ObjectValueAssignment::Colon),
        _ => cut_err(fail)
            .context(Context::Expression("object value assignment"))
            .context(Context::Expected(Expected::Char('=')))
            .context(Context::Expected(Expected::Char(':'))),
    }
    .parse_next(input)
}

fn for_expr_or_items<'i, F, I>(
    mut for_expr_parser: F,
    mut items_parser: I,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>>
where
    F: Parser<Input<'i>, (), ParseError<Input<'i>>>,
    I: Parser<Input<'i>, (), ParseError<Input<'i>>>,
{
    move |input: Input<'i>| {
        // The `for` tag needs to be followed by either a space character or a comment start to
        // disambiguate. Otherwise an identifier like `format` will match both the `for` tag
        // and the following identifier which would fail parsing of arrays with identifier/func
        // call elements and objects with those as keys.
        match peek((ws, b"for", one_of(" \t#/"))).parse_next(input) {
            Ok(_) => for_expr_parser.parse_next(input),
            Err(_) => items_parser.parse_next(input),
        }
    }
}

fn for_intro(input: Input) -> IResult<Input, ForIntro> {
    prefix_decorated(
        ws,
        delimited(
            b"for",
            (
                decorated(ws, cut_ident, ws),
                opt(preceded(b',', decorated(ws, cut_ident, ws))),
                preceded(cut_tag("in"), decorated(ws, expr, ws)),
            ),
            cut_char(':'),
        )
        .map(|(first, second, expr)| match second {
            Some(second) => {
                let mut intro = ForIntro::new(second, expr);
                intro.key_var = Some(first);
                intro
            }
            None => ForIntro::new(first, expr),
        }),
    )
    .parse_next(input)
}

fn for_cond(input: Input) -> IResult<Input, ForCond> {
    prefix_decorated(
        ws,
        preceded(b"if", decorated(ws, expr, ws)).map(ForCond::new),
    )
    .parse_next(input)
}

fn parenthesis<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        delimited(
            cut_char('('),
            decorated(ws, expr, ws)
                .map(|expr| state.borrow_mut().on_expr_term(Parenthesis::new(expr))),
            cut_char(')'),
        )
        .parse_next(input)
    }
}

fn heredoc<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        let (input, (indented, delim)) = heredoc_start(input)?;

        let (input, (template, trailing)) = (
            spanned(heredoc_template(delim)),
            terminated(
                raw_string(space0),
                cut_err(delim).context(Context::Expected(Expected::Description(
                    "heredoc end delimiter",
                ))),
            ),
        )
            .parse_next(input)?;

        let mut heredoc = HeredocTemplate::new(Ident::new_unchecked(delim), template);

        if indented {
            heredoc.dedent();
        }

        heredoc.set_trailing(trailing);

        state
            .borrow_mut()
            .on_expr_term(Expression::HeredocTemplate(Box::new(heredoc)));
        Ok((input, ()))
    }
}

fn heredoc_start(input: Input) -> IResult<Input, (bool, &str)> {
    terminated(
        (
            preceded(b"<<", opt(b'-')).map(|indent| indent.is_some()),
            cut_err(str_ident).context(Context::Expected(Expected::Description("identifier"))),
        ),
        cut_err(line_ending).context(Context::Expected(Expected::Char('\n'))),
    )
    .parse_next(input)
}

fn identlike<'i, 's>(
    state: &'s RefCell<ExprParseState>,
) -> impl Parser<Input<'i>, (), ParseError<Input<'i>>> + 's {
    move |input: Input<'i>| {
        (str_ident.with_span(), opt(prefix_decorated(ws, func_args)))
            .map(|((ident, span), func_args)| {
                let expr = match func_args {
                    Some(func_args) => {
                        let mut ident = Decorated::new(Ident::new_unchecked(ident));
                        ident.set_span(span);
                        let func_call = FuncCall::new(ident, func_args);
                        Expression::FuncCall(Box::new(func_call))
                    }
                    None => match ident {
                        "null" => Expression::Null(Null.into()),
                        "true" => Expression::Bool(true.into()),
                        "false" => Expression::Bool(false.into()),
                        var => Expression::Variable(Ident::new_unchecked(var).into()),
                    },
                };

                state.borrow_mut().on_expr_term(expr);
            })
            .parse_next(input)
    }
}

fn func_args(input: Input) -> IResult<Input, FuncArgs> {
    #[derive(Copy, Clone)]
    enum Trailer {
        Comma,
        Ellipsis,
    }

    let args = separated1(
        decorated(ws, preceded(peek(none_of(",.)")), expr), ws),
        b',',
    );

    let trailer = dispatch! {any;
        b',' => success(Trailer::Comma),
        b'.' => cut_tag("..").value(Trailer::Ellipsis),
        _ => fail,
    };

    delimited(
        b'(',
        (opt((args, opt(trailer))), raw_string(ws)).map(|(args, trailing)| {
            let mut args = match args {
                Some((args, Some(trailer))) => {
                    let args: Vec<_> = args;
                    let mut args = FuncArgs::from(args);
                    if let Trailer::Ellipsis = trailer {
                        args.set_expand_final(true);
                    } else {
                        args.set_trailing_comma(true);
                    }
                    args
                }
                Some((args, None)) => FuncArgs::from(args),
                None => FuncArgs::default(),
            };

            args.set_trailing(trailing);
            args
        }),
        cut_char(')'),
    )
    .parse_next(input)
}
