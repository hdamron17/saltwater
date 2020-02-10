use lazy_static::lazy_static;

use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;
use std::path::PathBuf;
use std::rc::Rc;

use codespan::FileId;

use super::{Lexer, Token};
use crate::data::error::CppError;
use crate::data::lex::{Keyword, Literal};
use crate::data::prelude::*;
use crate::get_str;
use crate::Files;

/// A preprocessor does textual substitution and deletion on a C source file.
///
/// The C preprocessor, or `cpp`, is tightly tied to C tokenization.
/// Rules for tokenizing identifiers, operators, and literals are all the same,
/// so you can't use it to preprocess e.g. Haskell, where `a'` is a valid identifier.
///
/// The preprocessor is further tied to the lexer because it is whitespace dependent:
/// `#define a() b` is _not_ the same as `#define a () b`.
/// The first is a function-like macro; the second is an object-like macro.
///
/// The preprocessor has no concept of scope: everything is either defined or not defined.
/// Variables can only be defined as strings, or more accurately, token sequences.
///
/// It is possible to tell the difference between an undefined variable
/// and a variable defined to be empty using
/// `#ifdef var` and `#if var`.
/// Note that `#if defined(...)` is not currently implemented.
///
/// Examples:
///
/// ```
/// use rcc::{Files, PreProcessor};
///
/// let mut files = Files::new();
/// let src = "int main(void) { char *hello = \"hi\"; }";
/// let file = files.add("example.c", String::from(src).into());
/// let cpp = PreProcessor::new(file, src, false, &mut files);
/// for token in cpp {
///     assert!(token.is_ok());
/// }
/// ```
pub struct PreProcessor<'a> {
    /// The preprocessor collaborates extremely closely with the lexer,
    /// since it sometimes needs to know if a token is followed by whitespace.
    first_lexer: Lexer,
    /// Each lexer represents a separate source file that is currently being processed.
    includes: Vec<Lexer>,
    /// All known files, including files which have already been read.
    files: &'a mut Files,
    /// Note that this is a simple HashMap and not a Scope, because
    /// the preprocessor has no concept of scope other than `undef`
    definitions: HashMap<InternedStr, Vec<Token>>,
    error_handler: ErrorHandler,
    /// Whether or not to display each token as it is processed
    debug: bool,
    /// Keeps track of the _start_ of all `#if` directives
    /// bool: whether we've seen an `else`
    nested_ifs: Vec<bool>,
    /// The tokens that have been `#define`d and are currently being substituted
    pending: VecDeque<Locatable<Token>>,
}

type CppResult<T> = Result<Locatable<T>, CompileError>;

macro_rules! ret_err {
    ($result: expr) => {
        match $result {
            Ok(data) => data,
            Err(err) => return Some(Err(err)),
        }
    };
}

impl Iterator for PreProcessor<'_> {
    /// The preprocessor hides all internal complexity and returns only tokens.
    type Item = CppResult<Token>;
    fn next(&mut self) -> Option<Self::Item> {
        let next_token = if let Some(err) = self.error_handler.pop_front() {
            Some(Err(err))
        } else if let Some(token) = self.pending.pop_front() {
            Some(Ok(token))
        } else {
            match self.next_cpp_token()? {
                Err(err) => return Some(Err(err)),
                Ok(loc) => match loc.data {
                    CppToken::Directive(directive) => {
                        let start = loc.location.span.start().to_usize() as u32;
                        self.directive(directive, start)
                    }
                    CppToken::Token(token) => self.handle_token(token, loc.location),
                },
            }
        };
        if self.debug {
            if let Some(Ok(token)) = &next_token {
                println!("token: {}", token.data);
            }
        }
        next_token
    }
}

// idiom: to check if there has been a newline since the last token,
// use the following pattern:
// ```rust
// let line = self.lexer.line;
// ... do stuff that consumes tokens ...
// let seen_newline = line == self.lexer.line;
// ```
impl<'a> PreProcessor<'a> {
    fn lexer(&self) -> &Lexer {
        self.includes.last().unwrap_or(&self.first_lexer)
    }
    fn lexer_mut(&mut self) -> &mut Lexer {
        self.includes.last_mut().unwrap_or(&mut self.first_lexer)
    }
    #[inline]
    fn line(&self) -> usize {
        self.lexer().line
    }
    #[inline]
    fn next_token(&mut self) -> Option<CppResult<Token>> {
        self.lexer_mut().next()
    }
    #[inline]
    fn peek_token(&mut self) -> Option<u8> {
        self.lexer_mut().peek()
    }
    #[inline]
    fn span(&self, start: u32) -> Location {
        self.lexer().span(start)
    }
    #[inline]
    fn consume_whitespace(&mut self) {
        self.lexer_mut().consume_whitespace()
    }
    #[inline]
    fn seen_line_token(&self) -> bool {
        self.lexer().seen_line_token
    }
    #[inline]
    fn offset(&self) -> u32 {
        self.lexer().location.offset
    }
    // possibly recursively replace tokens
    fn handle_token(&mut self, token: Token, location: Location) -> Option<CppResult<Token>> {
        if let Token::Id(id) = token {
            let mut token = self.replace_id(id, location);
            if let Some(Ok(Locatable {
                data: data @ Token::Id(_),
                ..
            })) = &mut token
            {
                if let Token::Id(name) = &data {
                    if let Some(keyword) = KEYWORDS.get(get_str!(name)) {
                        *data = Token::Keyword(*keyword);
                    }
                }
            }
            token
        } else {
            Some(Ok(Locatable::new(token, location)))
        }
    }
    /// Wrapper around [`Lexer::new`]
    pub fn new<S: Into<Rc<str>>>(
        file: FileId,
        chars: S,
        debug: bool,
        files: &'a mut Files,
    ) -> Self {
        Self {
            debug,
            first_lexer: Lexer::new(file, chars),
            includes: Default::default(),
            definitions: Default::default(),
            error_handler: Default::default(),
            nested_ifs: Default::default(),
            pending: Default::default(),
            files,
        }
    }
    /// Return the first valid token in the file,
    /// or None if there are no valid tokens.
    ///
    /// In either case, return all invalid tokens found.
    pub fn first_token(&mut self) -> (Option<Locatable<Token>>, VecDeque<CompileError>) {
        let mut errs = VecDeque::new();
        loop {
            match self.next() {
                Some(Ok(token)) => return (Some(token), errs),
                Some(Err(err)) => errs.push_back(err),
                None => return (None, errs),
            }
        }
    }

    /// Return all warnings found so far.
    ///
    /// These warnings are consumed and will not be returned if you call
    /// `warnings()` again.
    pub fn warnings(&mut self) -> VecDeque<CompileWarning> {
        std::mem::replace(&mut self.error_handler.warnings, Default::default())
    }

    /* internal functions */
    fn tokens_until_newline(&mut self) -> Vec<CompileResult<Locatable<Token>>> {
        let mut tokens = Vec::new();
        let line = self.line();
        loop {
            self.consume_whitespace();
            if self.line() != line {
                break;
            }
            match self.next_token() {
                Some(token) => tokens.push(token),
                None => break,
            }
        }
        tokens
    }

    fn next_cpp_token(&mut self) -> Option<CppResult<CppToken>> {
        let next_token = loop {
            // we have to duplicate a bit of code here to avoid borrow errors
            let lexer = self.includes.last_mut().unwrap_or(&mut self.first_lexer);
            match lexer.next() {
                Some(token) => break token,
                None => {
                    self.error_handler.append(&mut lexer.error_handler);
                    // this is the original source file
                    if self.includes.is_empty() {
                        return None;
                    } else {
                        self.includes.pop();
                    }
                }
            }
        };
        let is_hash = match next_token {
            Ok(Locatable {
                data: Token::Hash, ..
            }) => true,
            _ => false,
        };
        Some(if is_hash && !self.seen_line_token() {
            let line = self.line();
            match self.next_token()? {
                Ok(Locatable {
                    data: Token::Id(id),
                    location,
                }) if self.line() == line => {
                    if let Ok(directive) = DirectiveKind::try_from(get_str!(id)) {
                        Ok(Locatable::new(CppToken::Directive(directive), location))
                    } else {
                        Err(Locatable::new(CppError::InvalidDirective.into(), location))
                    }
                }
                Ok(other) if self.line() == line => {
                    Err(other.map(|tok| CppError::UnexpectedToken("directive", tok).into()))
                }
                other => other.map(Locatable::from),
            }
        } else {
            next_token.map(Locatable::from)
        })
    }
    // this function does _not_ perform macro substitution
    fn expect_id(&mut self) -> CppResult<InternedStr> {
        let location = self.span(self.offset());
        match self.next_token() {
            Some(Ok(Locatable {
                data: Token::Id(name),
                location,
            })) => Ok(Locatable::new(name, location)),
            Some(Err(err)) => Err(err),
            Some(Ok(other)) => {
                Err(other.map(|tok| CppError::UnexpectedToken("identifier", tok).into()))
            }
            None => Err(CompileError {
                data: CppError::EndOfFile("identifier").into(),
                location,
            }),
        }
    }
    fn directive(&mut self, kind: DirectiveKind, start: u32) -> Option<CppResult<Token>> {
        use crate::data::error::Warning as WarningDiagnostic;
        use DirectiveKind::*;
        match kind {
            If => {
                let condition = ret_err!(self.boolean_expr());
                self.if_directive(condition, start)
            }
            IfNDef => {
                let name = ret_err!(self.expect_id());
                self.if_directive(!self.definitions.contains_key(&name.data), start)
            }
            IfDef => {
                let name = ret_err!(self.expect_id());
                self.if_directive(self.definitions.contains_key(&name.data), start)
            }
            Else => match self.nested_ifs.last() {
                None => Some(Err(CompileError::new(
                    CppError::UnexpectedElse.into(),
                    self.span(start),
                ))),
                // we already took the `#if` condition,
                // `#else` should just be ignored
                Some(true) => {
                    ret_err!(self.consume_directive(start, DirectiveKind::Else));
                    self.next()
                }
                // we saw an `#else` before, seeing it again is an error
                Some(false) => Some(Err(CompileError::new(
                    CppError::UnexpectedElse.into(),
                    self.span(start),
                ))),
            },
            EndIf => {
                if self.nested_ifs.pop().is_none() {
                    Some(Err(CompileError::new(
                        CppError::UnexpectedEndIf.into(),
                        self.span(start),
                    )))
                } else {
                    self.next()
                }
            }
            Define => {
                ret_err!(self.define(start));
                self.next()
            }
            Undef => {
                let name = ret_err!(self.expect_id());
                self.definitions.remove(&name.data);
                self.next()
            }
            Pragma => {
                self.error_handler
                    .warn(WarningDiagnostic::IgnoredPragma, self.span(start));
                drop(self.tokens_until_newline());
                self.next()
            }
            // NOTE: #warning is a non-standard extension, but is implemented
            // by most major compilers including clang and gcc.
            Warning => {
                let tokens: Vec<_> = ret_err!(self
                    .tokens_until_newline()
                    .into_iter()
                    .map(|res| res.map(|l| l.data))
                    .collect());
                self.error_handler
                    .warn(WarningDiagnostic::User(tokens), self.span(start));
                self.next()
            }
            Error => {
                let tokens: Vec<_> = ret_err!(self
                    .tokens_until_newline()
                    .into_iter()
                    .map(|res| res.map(|l| l.data))
                    .collect());
                self.error_handler
                    .error(CppError::User(tokens), self.span(start));
                self.next()
            }
            Line => {
                self.error_handler.warn(
                    WarningDiagnostic::Generic("#line is not yet implemented".into()),
                    self.span(start),
                );
                drop(self.tokens_until_newline());
                self.next()
            }
            Include => {
                ret_err!(self.include(start));
                self.next()
            }
        }
    }
    // TODO: this needs to have an idea of 'pending chars', not just pending tokens
    fn replace_id(
        &mut self,
        mut name: InternedStr,
        location: Location,
    ) -> Option<CppResult<Token>> {
        let start = self.offset();
        while let Some(def) = self.definitions.get(&name) {
            if def.is_empty() {
                // TODO: recursion is bad and I should feel bad
                return self.next();
            }
            let first = &def[0];

            if def.len() > 1 {
                // prepend the new tokens to the pending tokens
                let mut new_pending = VecDeque::new();
                new_pending.extend(def[1..].iter().map(|token| Locatable {
                    data: token.clone(),
                    location,
                }));
                new_pending.append(&mut self.pending);
                self.pending = new_pending;
            }

            if let Token::Id(new_name) = first {
                name = *new_name;
            } else {
                return Some(Ok(Locatable::new(first.clone(), self.span(start))));
            }
        }
        Some(Ok(Locatable::new(Token::Id(name), self.span(start))))
    }
    // convienience function around cpp_expr
    fn boolean_expr(&mut self) -> Result<bool, CompileError> {
        // TODO: is this unwrap safe? there should only be scalar types in a cpp directive...
        match self.cpp_expr()?.truthy().unwrap().constexpr()?.data {
            (Literal::Int(i), Type::Bool) => Ok(i != 0),
            _ => unreachable!("bug in const_fold or parser: cpp cond should be boolean"),
        }
    }
    /// A C expression on a single line. Used for `#if` directives.
    ///
    /// Note that identifiers are replaced with a constant 0,
    /// as per [6.10.1](http://port70.net/~nsz/c/c11/n1570.html#6.10.1p4).
    fn cpp_expr(&mut self) -> Result<Expr, CompileError> {
        let start = self.offset();
        let mut line_tokens = self
            .tokens_until_newline()
            .into_iter()
            .map(|result| match result {
                Ok(Locatable {
                    data: Token::Id(_),
                    location,
                }) => Ok(location.with(Token::Literal(Literal::Int(0)))),
                _ => result,
            });
        // NOTE: This only returns the first error because anything else requires a refactor
        let first = line_tokens.next().unwrap_or_else(|| {
            Err(CompileError::new(
                CppError::EmptyExpression.into(),
                self.span(start),
            ))
        })?;
        let mut parser = crate::Parser::new(first, line_tokens, self.debug);
        // TODO: catch expressions that aren't allowed
        // (see https://github.com/jyn514/rcc/issues/5#issuecomment-575339427)
        // TODO: can semantic errors happen here? should we check?
        parser.expr().map_err(CompileError::from)
    }
    /// We've already seen an `#if` or `#ifdef` and are processing the
    /// lines that follow.
    fn if_directive(&mut self, condition: bool, start: u32) -> Option<CppResult<Token>> {
        if condition {
            self.nested_ifs.push(true);
        } else {
            ret_err!(self.consume_directive(start, DirectiveKind::If));
        }
        self.next()
    }
    /// Assuming we've just seen `#if 0`, keep consuming tokens until `#endif`
    /// This has to take into account nesting of #if directives.
    ///
    /// Example:
    /// ```c
    /// # if 0
    /// # if 1
    ///   int main() {}
    /// # endif
    /// void f() {}
    /// # endif
    /// int g() { return 0; }
    /// ```
    /// should yield `int` as the next token, not `void`.
    fn consume_directive(&mut self, start: u32, kind: DirectiveKind) -> Result<(), CompileError> {
        fn match_directive(
            token: &CppResult<CppToken>,
            expected: DirectiveKind,
        ) -> Option<Location> {
            match token {
                Ok(Locatable {
                    data: CppToken::Directive(directive),
                    location,
                }) => {
                    if *directive == expected {
                        Some(*location)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
        let mut depth = 1;
        while depth > 0 {
            let token = match self.next_cpp_token() {
                Some(token) => token,
                None => {
                    return Err(Locatable::new(
                        CppError::UnterminatedIf {
                            ifdef: kind == DirectiveKind::IfDef,
                        },
                        self.span(start),
                    )
                    .into())
                }
            };
            if match_directive(&token, DirectiveKind::If).is_some()
                || match_directive(&token, DirectiveKind::IfDef).is_some()
            {
                depth += 1;
            } else if match_directive(&token, DirectiveKind::EndIf).is_some() {
                depth -= 1;
            } else if let Some(location) = match_directive(&token, DirectiveKind::Else) {
                if kind == DirectiveKind::If {
                    if depth == 1 {
                        self.nested_ifs.push(false);
                        break;
                    }
                } else {
                    assert_eq!(kind, DirectiveKind::Else);
                    self.nested_ifs.pop();
                    return Err(CompileError::new(CppError::UnexpectedElse.into(), location));
                }
                // otherwise, discard #else
            }
        }
        Ok(())
    }
    fn define(&mut self, start: u32) -> Result<(), Locatable<Error>> {
        let line = self.line();
        self.consume_whitespace();
        if self.line() != line {
            return Err(self.span(start).error(CppError::EmptyDefine));
        }
        let id = self.expect_id()?;
        if self.peek_token() == Some(b'(') {
            // function macro
            unimplemented!("function macros")
        } else {
            // object macro
            let tokens = self
                .tokens_until_newline()
                .into_iter()
                .map(|res| res.map(|loc| loc.data))
                .collect::<Result<_, Locatable<Error>>>()?;
            self.definitions.insert(id.data, tokens);
            Ok(())
        }
    }
    /*
    fn match_token(&mut self, token: Token) -> Option<Locatable<Token>> {
        use std::mem;
        let next_token = self.peek_token()?;
        if mem::discriminant(next_token) == mem::discriminant(token) {
            self.next_token()
        } else {
            None
        }
    }
    */
    fn include(&mut self, start: u32) -> Result<(), Locatable<Error>> {
        use crate::data::lex::{ComparisonToken, Literal};
        let lexer = self.lexer_mut();
        lexer.consume_whitespace();
        let local = if lexer.match_next(b'"') {
            true
        } else if lexer.match_next(b'<') {
            false
        } else {
            let (id, location) = match self.next_token() {
                Some(Ok(Locatable {
                    data: Token::Id(id),
                    location,
                })) => (id, location),
                Some(Err(err)) => return Err(err),
                Some(Ok(other)) => {
                    return Err(CompileError::new(
                        CppError::UnexpectedToken("include file", other.data).into(),
                        other.location,
                    ))
                }
                None => {
                    return Err(CompileError::new(
                        CppError::EndOfFile("include file").into(),
                        self.span(start),
                    ))
                }
            };
            match self.replace_id(id, location) {
                // local
                Some(Ok(Locatable {
                    data: Token::Literal(Literal::Str(_)),
                    ..
                })) => unimplemented!("#include for macros"), //return self.include_path(id, true, start),
                // system
                Some(Ok(Locatable {
                    data: Token::Comparison(ComparisonToken::Less),
                    ..
                })) => false,
                Some(Ok(other)) => {
                    return Err(CompileError::new(
                        CppError::UnexpectedToken("include file", other.data).into(),
                        other.location,
                    ))
                }
                Some(Err(err)) => return Err(err),
                None => {
                    return Err(CompileError::new(
                        CppError::EndOfFile("include file").into(),
                        self.span(start),
                    ))
                }
            }
        };

        let filename = self.bytes_until(if local { b'"' } else { b'>' });
        self.include_path(filename, local, start)
    }
    fn include_path(
        &mut self,
        filename: Vec<u8>,
        local: bool,
        start: u32,
    ) -> Result<(), Locatable<Error>> {
        const SEARCH_PATH: &[&str] = &["/usr/include"];
        log::debug!("in search path");

        // Recall that the original file was valid UTF8.
        // Since in UTF8, no ASCII character can occur
        // within a multi-byte sequence, `filename` must be valid UTF8.
        let filename = String::from_utf8(filename).expect("passed invalid utf8 to start");

        // local include: #include "dict.h"
        if local {
            // TODO: relative file paths
            unimplemented!();
        }
        for path in SEARCH_PATH {
            let mut buf = PathBuf::from(path);
            buf.push(&filename);
            if buf.exists() {
                // TODO: _any_ sort of error handling
                let src = std::fs::read_to_string(&buf)
                    .expect("failed to read included file")
                    .into();
                let id = self.files.add(buf.to_string_lossy(), Rc::clone(&src));
                self.includes.push(Lexer::new(id, src));
                return Ok(());
            }
        }
        return Err(CompileError::new(
            CppError::FileNotFound(filename).into(),
            self.span(start),
        ));
    }
    fn bytes_until(&mut self, byte: u8) -> Vec<u8> {
        log::debug!("in bytes_until");
        let mut bytes = Vec::new();
        while self.lexer_mut().peek() != Some(byte) {
            match self.lexer_mut().next_char() {
                None => return bytes,
                Some(c) => bytes.push(c),
            }
        }
        bytes
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum DirectiveKind {
    If,
    EndIf,
    Else,
    IfDef,
    IfNDef,
    Include,
    Define,
    Undef,
    Line,
    Warning,
    Error,
    Pragma,
}

#[derive(Clone, Debug, PartialEq)]
enum CppToken {
    Token(Token),
    Directive(DirectiveKind),
}

impl From<Locatable<Token>> for Locatable<CppToken> {
    fn from(token: Locatable<Token>) -> Locatable<CppToken> {
        Locatable::new(CppToken::Token(token.data), token.location)
    }
}

impl TryFrom<&str> for DirectiveKind {
    type Error = ();
    fn try_from(s: &str) -> Result<Self, ()> {
        use DirectiveKind::*;
        Ok(match s {
            "if" => If,
            "endif" => EndIf,
            "else" => Else,
            "ifdef" => IfDef,
            "ifndef" => IfNDef,
            "include" => Include,
            "define" => Define,
            "undef" => Undef,
            "line" => Line,
            "warning" => Warning,
            "error" => Error,
            "pragma" => Pragma,
            _ => return Err(()),
        })
    }
}

lazy_static! {
    static ref KEYWORDS: HashMap<&'static str, Keyword> = map!{
        // control flow
        "if" => Keyword::If,
        "else" => Keyword::Else,
        "do" => Keyword::Do,
        "while" => Keyword::While,
        "for" => Keyword::For,
        "switch" => Keyword::Switch,
        "case" => Keyword::Case,
        "default" => Keyword::Default,
        "break" => Keyword::Break,
        "continue" => Keyword::Continue,
        "return" => Keyword::Return,
        "goto" => Keyword::Goto,

        // types
        "__builtin_va_list" => Keyword::VaList,
        "_Bool" => Keyword::Bool,
        "char" => Keyword::Char,
        "short" => Keyword::Short,
        "int" => Keyword::Int,
        "long" => Keyword::Long,
        "float" => Keyword::Float,
        "double" => Keyword::Double,
        "_Complex" => Keyword::Complex,
        "_Imaginary" => Keyword::Imaginary,
        "void" => Keyword::Void,
        "signed" => Keyword::Signed,
        "unsigned" => Keyword::Unsigned,
        "typedef" => Keyword::Typedef,
        "enum" => Keyword::Enum,
        "union" => Keyword::Union,
        "struct" => Keyword::Struct,

        // qualifiers
        "const" => Keyword::Const,
        "volatile" => Keyword::Volatile,
        "restrict" => Keyword::Restrict,
        "_Atomic" => Keyword::Atomic,
        "_Thread_local" => Keyword::ThreadLocal,

        // function qualifiers
        "inline" => Keyword::Inline,
        "_Noreturn" => Keyword::NoReturn,

        // storage classes
        "auto" => Keyword::Auto,
        "register" => Keyword::Register,
        "static" => Keyword::Static,
        "extern" => Keyword::Extern,

        // compiler intrinsics
        "sizeof" => Keyword::Sizeof,
        "_Alignof" => Keyword::Alignof,
        "_Alignas" => Keyword::Alignas,
        "_Generic" => Keyword::Generic,
        "_Static_assert" => Keyword::StaticAssert,
    };
}

#[cfg(test)]
mod tests {
    use super::{CppError, CppResult, Keyword, PreProcessor, KEYWORDS};
    use crate::data::prelude::*;
    fn cpp(input: &str) -> PreProcessor {
        PreProcessor::new("<test suite>", input, false)
    }
    macro_rules! assert_err {
        ($src: expr, $err: pat, $description: expr $(,)?) => {
            match cpp($src).next().unwrap().unwrap_err().data {
                Error::PreProcessor($err) => {}
                Error::PreProcessor(other) => panic!("expected {}, got {}", $description, other),
                _ => panic!("expected cpp err"),
            }
        };
    }
    fn assert_keyword(token: Option<CppResult<Token>>, expected: Keyword) {
        match token {
            Some(Ok(Locatable {
                data: Token::Keyword(actual),
                ..
            })) => assert_eq!(actual, expected),
            _ => panic!("not a keyword: {:?}", token),
        }
    }
    fn assert_same(src: &str, cpp_src: &str) {
        assert_eq!(
            cpp(src)
                .map(|res| res.map(|token| token.data))
                .collect::<Vec<_>>(),
            cpp(cpp_src)
                .map(|res| res.map(|token| token.data))
                .collect::<Vec<_>>(),
            "{} is not the same as {}",
            src,
            cpp_src,
        );
    }
    #[test]
    fn keywords() {
        for keyword in KEYWORDS.values() {
            // va_list is usually a typedef to `__builtin_va_list`
            // and making it a keyword messes up parsing
            if *keyword != Keyword::VaList {
                println!("{}", keyword);
                assert_keyword(cpp(&keyword.to_string()).next(), *keyword);
            }
        }
    }
    #[test]
    fn if_directive() {
        assert_same(
            "
#if a
    b
#else
    c
#endif",
            "c",
        );
        assert_same(
            "
#if 0 + 2
    b
#endif",
            "b",
        );
        assert_same(
            "
#if 1^1
    b
#endif",
            "",
        );
    }
    #[test]
    fn ifdef() {
        let code = "#ifdef a
        whatever, doesn't matter
        #endif";
        assert_eq!(cpp(code).next(), None);

        let code = "#ifdef a\n#endif";
        assert_eq!(cpp(code).next(), None);

        assert!(cpp("#ifdef").next().unwrap().is_err());

        let nested = "#ifdef a
        #ifdef b
        int main() {}
        #endif
        #endif
        char;";
        assert_eq!(
            cpp(nested).next().unwrap().unwrap().data,
            Token::Keyword(Keyword::Char)
        );

        assert!(cpp("#endif").next().unwrap().is_err());

        let same_line = "#ifdef a #endif\nint main() {}";
        assert!(cpp(same_line).next().unwrap().is_err());
    }
    #[test]
    fn ifndef() {
        let src = "
#ifndef A
#define A
#endif
A";
        assert!(cpp(src).next().is_none());
    }
    #[test]
    fn object_macros() {
        let src = "
#define a b
int a() { return 1; }";
        let cpp_src = "int b() { return 1; }";
        assert_same(src, cpp_src);

        let multidef = "
#define a b + c
int d() { return a; }";
        assert_same(multidef, "int d() { return b + c; }");

        let opdef = "
#define BEGIN {
#define END }
int f() BEGIN return 5; END";
        assert_same(opdef, "int f() { return 5; }");
    }
    #[test]
    fn empty_def() {
        assert_err!("#define", CppError::EndOfFile(_), "empty define",);
        assert_err!(
            "#define
            int",
            CppError::EmptyDefine,
            "empty define",
        );
    }
    #[test]
    fn undef() {
        let src = "
#define a b
a
#undef a
a";
        let cpp_src = "
b
a";
        assert_same(src, cpp_src);
    }
    #[test]
    fn else_directive() {
        use super::CppError;
        let src = "
#if 1
#if 0
b
#else
// this should be an error
#else
d
#endif
";
        assert_err!(src, CppError::UnexpectedElse, "duplicate else",);
    }
    #[test]
    fn pragma() {
        let src = "#pragma gcc __attribute__((inline))";
        assert!(cpp(src).next().is_none());
    }
    #[test]
    fn line() {
        let src = "#line 1";
        let mut cpp = cpp(src);
        assert!(cpp.next().is_none());
        assert!(cpp.warnings().pop_front().is_some());
    }
    #[test]
    fn warning() {
        let src = "#warning your pants are on file";
        let mut cpp = cpp(src);
        assert!(cpp.next().is_none());
        assert!(cpp.warnings().pop_front().is_some());
    }
    #[test]
    fn error() {
        assert_err!("#error cannot drink and drive", CppError::User(_), "#error",);
    }
    #[test]
    fn invalid_directive() {
        assert_err!("#wrong", CppError::InvalidDirective, "invalid directive",);
        assert_err!("#1", CppError::UnexpectedToken(_, _), "unexpected token",);
    }
}
