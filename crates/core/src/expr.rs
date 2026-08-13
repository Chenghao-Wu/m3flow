//! Reference expressions `${path.to.value}` and the condition mini-language.
//!
//! References bind step inputs to workflow inputs / other steps' outputs.
//! Conditions gate node execution (v0.2 control flow, plan §37).
//!
//! Grammar of conditions:
//!   or      := and ("or" and)*
//!   and     := cmp ("and" cmp)*
//!   cmp     := unary (("=="|"!="|"<="|">="|"<"|">") unary)?
//!   unary   := "not" unary | primary
//!   primary := literal | ref | "(" or ")"
//!   ref     := "${" ident ("." ident)* "}"
//!   literal := "true" | "false" | "null" | number | 'string' | "string"

use crate::error::{M3FlowError, Result};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Reference {
    pub path: Vec<String>,
}

impl Reference {
    pub fn parse(s: &str) -> Result<Self> {
        let body = s
            .strip_prefix("${")
            .and_then(|x| x.strip_suffix('}'))
            .ok_or_else(|| M3FlowError::schema(format!("malformed reference '{s}'")))?;
        let path: Vec<String> = body.split('.').map(|p| p.trim().to_string()).collect();
        if path.is_empty() || path.iter().any(|p| p.is_empty()) {
            return Err(M3FlowError::schema(format!("malformed reference '{s}'")));
        }
        Ok(Self { path })
    }

    /// If the whole string is exactly one `${...}`, return it.
    pub fn whole(s: &str) -> Option<Self> {
        let t = s.trim();
        if t.starts_with("${") && t.ends_with('}') && t[2..].find('{').is_none() {
            Self::parse(t).ok()
        } else {
            None
        }
    }

    pub fn display(&self) -> String {
        format!("${{{}}}", self.path.join("."))
    }
}

/// Collect every reference occurring in a JSON value (walking strings).
pub fn find_references(v: &serde_json::Value, out: &mut Vec<Reference>) {
    match v {
        serde_json::Value::String(s) => {
            let mut rest = s.as_str();
            while let Some(start) = rest.find("${") {
                if let Some(end) = rest[start..].find('}') {
                    let cand = &rest[start..start + end + 1];
                    if let Ok(r) = Reference::parse(cand) {
                        out.push(r);
                    }
                    rest = &rest[start + end + 1..];
                } else {
                    break;
                }
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| find_references(x, out)),
        serde_json::Value::Object(m) => m.values().for_each(|x| find_references(x, out)),
        _ => {}
    }
}

// ------------------------------------------------------- condition language

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Ref(Vec<String>),
    Num(f64),
    Str(String),
    True,
    False,
    Null,
    And,
    Or,
    Not,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    LParen,
    RParen,
}

fn lex(s: &str) -> Result<Vec<Tok>> {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            '=' => {
                if b.get(i + 1) == Some(&'=') {
                    out.push(Tok::Eq);
                    i += 2;
                } else {
                    return Err(M3FlowError::schema("use '==' for equality in conditions"));
                }
            }
            '!' => {
                if b.get(i + 1) == Some(&'=') {
                    out.push(Tok::Ne);
                    i += 2;
                } else {
                    return Err(M3FlowError::schema("use 'not' for negation in conditions"));
                }
            }
            '<' => {
                if b.get(i + 1) == Some(&'=') {
                    out.push(Tok::Le);
                    i += 2;
                } else {
                    out.push(Tok::Lt);
                    i += 1;
                }
            }
            '>' => {
                if b.get(i + 1) == Some(&'=') {
                    out.push(Tok::Ge);
                    i += 2;
                } else {
                    out.push(Tok::Gt);
                    i += 1;
                }
            }
            '$' => {
                if b.get(i + 1) == Some(&'{') {
                    let mut j = i + 2;
                    let mut path = String::new();
                    while j < b.len() && b[j] != '}' {
                        path.push(b[j]);
                        j += 1;
                    }
                    if j >= b.len() {
                        return Err(M3FlowError::schema("unterminated ${...} in condition"));
                    }
                    let parts: Vec<String> =
                        path.split('.').map(|p| p.trim().to_string()).collect();
                    out.push(Tok::Ref(parts));
                    i = j + 1;
                } else {
                    return Err(M3FlowError::schema("expected '${' in condition"));
                }
            }
            '"' | '\'' => {
                let mut j = i + 1;
                let mut val = String::new();
                while j < b.len() && b[j] != c {
                    val.push(b[j]);
                    j += 1;
                }
                if j >= b.len() {
                    return Err(M3FlowError::schema("unterminated string in condition"));
                }
                out.push(Tok::Str(val));
                i = j + 1;
            }
            _ if c.is_ascii_digit() || c == '-' || c == '.' => {
                let mut j = i;
                let mut num = String::new();
                while j < b.len() && (b[j].is_ascii_digit() || matches!(b[j], '.' | '-' | '+' | 'e' | 'E')) {
                    num.push(b[j]);
                    j += 1;
                }
                let v: f64 = num
                    .parse()
                    .map_err(|_| M3FlowError::schema(format!("bad number '{num}' in condition")))?;
                out.push(Tok::Num(v));
                i = j;
            }
            _ if c.is_alphabetic() || c == '_' => {
                let mut j = i;
                let mut word = String::new();
                while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_') {
                    word.push(b[j]);
                    j += 1;
                }
                out.push(match word.as_str() {
                    "and" => Tok::And,
                    "or" => Tok::Or,
                    "not" => Tok::Not,
                    "true" => Tok::True,
                    "false" => Tok::False,
                    "null" => Tok::Null,
                    _ => Tok::Ident(word),
                });
                i = j;
            }
            other => {
                return Err(M3FlowError::schema(format!(
                    "unexpected character '{other}' in condition"
                )))
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Clone)]
enum Node {
    Lit(serde_json::Value),
    Ref(Vec<String>),
    Not(Box<Node>),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Cmp(Box<Node>, CmpOp, Box<Node>),
}

#[derive(Debug, Clone, Copy)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn parse_or(&mut self) -> Result<Node> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek(), Some(Tok::Or)) {
            self.next();
            let rhs = self.parse_and()?;
            lhs = Node::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Node> {
        let mut lhs = self.parse_cmp()?;
        while matches!(self.peek(), Some(Tok::And)) {
            self.next();
            let rhs = self.parse_cmp()?;
            lhs = Node::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_cmp(&mut self) -> Result<Node> {
        let lhs = self.parse_unary()?;
        let op = match self.peek() {
            Some(Tok::Eq) => Some(CmpOp::Eq),
            Some(Tok::Ne) => Some(CmpOp::Ne),
            Some(Tok::Lt) => Some(CmpOp::Lt),
            Some(Tok::Le) => Some(CmpOp::Le),
            Some(Tok::Gt) => Some(CmpOp::Gt),
            Some(Tok::Ge) => Some(CmpOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.next();
            let rhs = self.parse_unary()?;
            Ok(Node::Cmp(Box::new(lhs), op, Box::new(rhs)))
        } else {
            Ok(lhs)
        }
    }

    fn parse_unary(&mut self) -> Result<Node> {
        if matches!(self.peek(), Some(Tok::Not)) {
            self.next();
            return Ok(Node::Not(Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Node> {
        match self.next() {
            Some(Tok::True) => Ok(Node::Lit(serde_json::Value::Bool(true))),
            Some(Tok::False) => Ok(Node::Lit(serde_json::Value::Bool(false))),
            Some(Tok::Null) => Ok(Node::Lit(serde_json::Value::Null)),
            Some(Tok::Num(n)) => Ok(Node::Lit(serde_json::json!(n))),
            Some(Tok::Str(s)) => Ok(Node::Lit(serde_json::Value::String(s))),
            Some(Tok::Ref(p)) => Ok(Node::Ref(p)),
            Some(Tok::Ident(s)) => Ok(Node::Lit(serde_json::Value::String(s))),
            Some(Tok::LParen) => {
                let n = self.parse_or()?;
                match self.next() {
                    Some(Tok::RParen) => Ok(n),
                    _ => Err(M3FlowError::schema("missing ')' in condition")),
                }
            }
            other => Err(M3FlowError::schema(format!(
                "unexpected token {other:?} in condition"
            ))),
        }
    }
}

fn truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Null => false,
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
        serde_json::Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn eval(node: &Node, resolve: &dyn Fn(&[String]) -> Option<serde_json::Value>) -> Result<serde_json::Value> {
    Ok(match node {
        Node::Lit(v) => v.clone(),
        Node::Ref(p) => resolve(p).ok_or_else(|| {
            M3FlowError::workflow(format!("cannot resolve ${{{}}} in condition", p.join(".")), None)
        })?,
        Node::Not(n) => serde_json::Value::Bool(!truthy(&eval(n, resolve)?)),
        Node::And(a, b) => serde_json::Value::Bool(
            truthy(&eval(a, resolve)?) && truthy(&eval(b, resolve)?),
        ),
        Node::Or(a, b) => serde_json::Value::Bool(
            truthy(&eval(a, resolve)?) || truthy(&eval(b, resolve)?),
        ),
        Node::Cmp(a, op, b) => {
            let l = eval(a, resolve)?;
            let r = eval(b, resolve)?;
            let res = match (&l, &r) {
                                (serde_json::Value::Number(x), serde_json::Value::Number(y)) => {
                    let (x, y) = (x.as_f64().unwrap_or(f64::NAN), y.as_f64().unwrap_or(f64::NAN));
                    match op {
                        CmpOp::Eq => x == y,
                        CmpOp::Ne => x != y,
                        CmpOp::Lt => x < y,
                        CmpOp::Le => x <= y,
                        CmpOp::Gt => x > y,
                        CmpOp::Ge => x >= y,
                    }
                }
                _ => match op {
                    CmpOp::Eq => l == r,
                    CmpOp::Ne => l != r,
                    _ => {
                        return Err(M3FlowError::workflow(
                            format!("ordering comparison needs numbers, got {l} and {r}"),
                            None,
                        ))
                    }
                },
            };
            serde_json::Value::Bool(res)
        }
    })
}

/// Evaluate a boolean condition expression against a path resolver.
pub fn eval_condition(
    src: &str,
    resolve: &dyn Fn(&[String]) -> Option<serde_json::Value>,
) -> Result<bool> {
    let toks = lex(src)?;
    if toks.is_empty() {
        return Err(M3FlowError::schema("empty condition"));
    }
    let mut p = Parser { toks, pos: 0 };
    let ast = p.parse_or()?;
    if p.pos != p.toks.len() {
        return Err(M3FlowError::schema("trailing tokens in condition"));
    }
    Ok(truthy(&eval(&ast, resolve)?))
}

/// Just parse + collect references used by a condition (for dependency edges).
pub fn condition_references(src: &str) -> Result<Vec<Reference>> {
    let toks = lex(src)?;
    let mut out = Vec::new();
    for t in &toks {
        if let Tok::Ref(p) = t {
            out.push(Reference { path: p.clone() });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn whole_reference() {
        let r = Reference::whole("${build.system}").unwrap();
        assert_eq!(r.path, vec!["build", "system"]);
        assert!(Reference::whole("pre-${x}").is_none());
        assert!(Reference::whole("${}").is_none());
    }

    #[test]
    fn find_refs_nested() {
        let v = json!({"a": "${x.y}", "b": ["see ${p.q} here", 3]});
        let mut out = Vec::new();
        find_references(&v, &mut out);
        assert_eq!(out.len(), 2);
    }

    fn ctx(pairs: &[(&str, serde_json::Value)]) -> impl Fn(&[String]) -> Option<serde_json::Value> {
        let m: HashMap<String, serde_json::Value> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        move |path: &[String]| m.get(&path.join(".")).cloned()
    }

    #[test]
    fn conditions() {
        let c = ctx(&[
            ("check.report.equilibrated", json!(true)),
            ("n", json!(5)),
            ("name", json!("npt")),
        ]);
        assert!(eval_condition("${check.report.equilibrated}", &c).unwrap());
        assert!(eval_condition("${check.report.equilibrated} == true", &c).unwrap());
        assert!(eval_condition("${n} >= 5 and ${name} == 'npt'", &c).unwrap());
        assert!(!eval_condition("not ${check.report.equilibrated}", &c).unwrap());
        assert!(eval_condition("(${n} < 3) or (${n} == 5)", &c).unwrap());
        assert!(eval_condition("${missing} == null", &ctx(&[])).is_err());
    }
}
