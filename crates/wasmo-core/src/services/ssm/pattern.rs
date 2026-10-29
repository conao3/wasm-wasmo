#[derive(Debug, Clone)]
enum ClassItem {
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

impl ClassItem {
    fn matches(&self, c: char) -> bool {
        match *self {
            ClassItem::Range(lo, hi) => lo <= c && c <= hi,
            ClassItem::Digit(neg) => c.is_ascii_digit() != neg,
            ClassItem::Word(neg) => (c.is_ascii_alphanumeric() || c == '_') != neg,
            ClassItem::Space(neg) => c.is_whitespace() != neg,
        }
    }
}

#[derive(Debug, Clone)]
enum Node {
    Char(char),
    Any,
    Class(Vec<ClassItem>, bool),
    Start,
    End,
    WordBoundary(bool),
    Alt(Vec<Vec<Node>>),
    Repeat {
        node: Box<Node>,
        min: usize,
        max: Option<usize>,
        greedy: bool,
    },
}

impl Node {
    fn single(&self, c: char) -> Option<bool> {
        match self {
            Node::Char(x) => Some(*x == c),
            Node::Any => Some(c != '\n'),
            Node::Class(items, neg) => Some(items.iter().any(|i| i.matches(c)) != *neg),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Pattern {
    root: Vec<Node>,
}

struct Parser<'a> {
    chars: Vec<char>,
    pos: usize,
    source: &'a str,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }

    fn fail<T>(&self, why: &str) -> Result<T, String> {
        Err(format!("{why} near index {} in {}", self.pos, self.source))
    }

    fn alternation(&mut self) -> Result<Vec<Vec<Node>>, String> {
        let mut alts = vec![self.sequence()?];
        while self.peek() == Some('|') {
            self.pos += 1;
            alts.push(self.sequence()?);
        }
        Ok(alts)
    }

    fn sequence(&mut self) -> Result<Vec<Node>, String> {
        let mut seq = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.atom()?;
            seq.push(self.quantified(atom)?);
        }
        Ok(seq)
    }

    fn quantified(&mut self, atom: Node) -> Result<Node, String> {
        let (min, max) = match self.peek() {
            Some('*') => (0, None),
            Some('+') => (1, None),
            Some('?') => (0, Some(1)),
            Some('{') if self.brace_ahead() => {
                self.pos += 1;
                let min = self.number().unwrap_or(0);
                let max = if self.peek() == Some(',') {
                    self.pos += 1;
                    self.number()
                } else {
                    Some(min)
                };
                if self.peek() != Some('}') {
                    return self.fail("Unclosed counted closure");
                }
                if max.is_some_and(|m| m < min) {
                    return self.fail("Illegal repetition range");
                }
                (min, max)
            }
            _ => return Ok(atom),
        };
        if matches!(atom, Node::Start | Node::End | Node::WordBoundary(_)) {
            return self.fail("Dangling meta character");
        }
        self.pos += 1;
        let greedy = if self.peek() == Some('?') {
            self.pos += 1;
            false
        } else {
            if self.peek() == Some('+') {
                self.pos += 1;
            }
            true
        };
        if matches!(self.peek(), Some('*' | '+' | '?')) {
            return self.fail("Dangling meta character");
        }
        Ok(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            greedy,
        })
    }

    fn brace_ahead(&self) -> bool {
        let rest: String = self.chars[self.pos + 1..].iter().collect();
        let Some(end) = rest.find('}') else {
            return false;
        };
        let body = &rest[..end];
        !body.is_empty()
            && body.chars().all(|c| c.is_ascii_digit() || c == ',')
            && body.matches(',').count() <= 1
            && !body.starts_with(',')
    }

    fn number(&mut self) -> Option<usize> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        let digits: String = self.chars[start..self.pos].iter().collect();
        digits.parse().ok()
    }

    fn atom(&mut self) -> Result<Node, String> {
        match self.next() {
            Some('^') => Ok(Node::Start),
            Some('$') => Ok(Node::End),
            Some('.') => Ok(Node::Any),
            Some('(') => {
                if self.peek() == Some('?') {
                    self.pos += 1;
                    if self.next() != Some(':') {
                        return self.fail("Unknown group type");
                    }
                }
                let alts = self.alternation()?;
                if self.next() != Some(')') {
                    return self.fail("Unclosed group");
                }
                Ok(Node::Alt(alts))
            }
            Some('[') => self.class(),
            Some('\\') => self.escape(),
            Some(c @ ('*' | '+' | '?')) => self.fail(&format!("Dangling meta character '{c}'")),
            Some(c) => Ok(Node::Char(c)),
            None => self.fail("Unexpected end"),
        }
    }

    fn escape(&mut self) -> Result<Node, String> {
        Ok(match self.next() {
            Some('b') => Node::WordBoundary(true),
            Some('B') => Node::WordBoundary(false),
            Some('A') => Node::Start,
            Some('z' | 'Z') => Node::End,
            _ => {
                self.pos -= 1;
                match self.class_escape()? {
                    ClassItem::Range(a, b) if a == b => Node::Char(a),
                    item => Node::Class(vec![item], false),
                }
            }
        })
    }

    fn class_escape(&mut self) -> Result<ClassItem, String> {
        let c = match self.next() {
            Some(c) => c,
            None => return self.fail("Unexpected internal error"),
        };
        let item = |c| ClassItem::Range(c, c);
        Ok(match c {
            'd' => ClassItem::Digit(false),
            'D' => ClassItem::Digit(true),
            'w' => ClassItem::Word(false),
            'W' => ClassItem::Word(true),
            's' => ClassItem::Space(false),
            'S' => ClassItem::Space(true),
            'n' => item('\n'),
            't' => item('\t'),
            'r' => item('\r'),
            'f' => item('\u{c}'),
            c if c.is_ascii_alphanumeric() => {
                return self.fail(&format!("Illegal/unsupported escape sequence \\{c}"));
            }
            c => item(c),
        })
    }

    fn class(&mut self) -> Result<Node, String> {
        let negated = self.peek() == Some('^');
        if negated {
            self.pos += 1;
        }
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let c = match self.next() {
                Some(c) => c,
                None => return self.fail("Unclosed character class"),
            };
            if c == ']' && !first {
                break;
            }
            first = false;
            let lo = if c == '\\' {
                match self.class_escape()? {
                    ClassItem::Range(a, _) => a,
                    other => {
                        items.push(other);
                        continue;
                    }
                }
            } else {
                c
            };
            if self.peek() == Some('-') && self.chars.get(self.pos + 1).is_some_and(|c| *c != ']') {
                self.pos += 1;
                let hi = match self.next() {
                    Some('\\') => match self.class_escape()? {
                        ClassItem::Range(a, _) => a,
                        _ => return self.fail("Illegal character range"),
                    },
                    Some(h) => h,
                    None => return self.fail("Unclosed character class"),
                };
                if hi < lo {
                    return self.fail("Illegal character range");
                }
                items.push(ClassItem::Range(lo, hi));
            } else {
                items.push(ClassItem::Range(lo, lo));
            }
        }
        Ok(Node::Class(items, negated))
    }
}

struct Matcher<'a> {
    text: &'a [char],
}

type Cont<'k> = &'k mut dyn FnMut(usize) -> bool;

impl Matcher<'_> {
    fn is_word(&self, pos: usize) -> bool {
        self.text
            .get(pos)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
    }

    fn seq(&self, seq: &[Node], pos: usize, k: Cont<'_>) -> bool {
        match seq.split_first() {
            None => k(pos),
            Some((first, rest)) => self.node(first, pos, &mut |p| self.seq(rest, p, k)),
        }
    }

    fn node(&self, node: &Node, pos: usize, k: Cont<'_>) -> bool {
        match node {
            Node::Start => pos == 0 && k(pos),
            Node::End => pos == self.text.len() && k(pos),
            Node::WordBoundary(want) => {
                let at = pos > 0 && self.is_word(pos - 1) != self.is_word(pos)
                    || pos == 0 && self.is_word(0);
                at == *want && k(pos)
            }
            Node::Alt(alts) => alts.iter().any(|alt| self.seq(alt, pos, k)),
            Node::Repeat {
                node,
                min,
                max,
                greedy,
            } => {
                if node.single('a').is_some() {
                    self.repeat_single(node, *min, *max, *greedy, pos, k)
                } else {
                    self.repeat(node, *min, *max, *greedy, 0, pos, k)
                }
            }
            single => {
                self.text
                    .get(pos)
                    .is_some_and(|c| single.single(*c) == Some(true))
                    && k(pos + 1)
            }
        }
    }

    fn repeat_single(
        &self,
        node: &Node,
        min: usize,
        max: Option<usize>,
        greedy: bool,
        pos: usize,
        k: Cont<'_>,
    ) -> bool {
        let limit = max.unwrap_or(usize::MAX);
        let mut n = 0;
        while n < limit
            && self
                .text
                .get(pos + n)
                .is_some_and(|c| node.single(*c) == Some(true))
        {
            n += 1;
        }
        if n < min {
            return false;
        }
        if greedy {
            (min..=n).rev().any(|j| k(pos + j))
        } else {
            (min..=n).any(|j| k(pos + j))
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn repeat(
        &self,
        node: &Node,
        min: usize,
        max: Option<usize>,
        greedy: bool,
        count: usize,
        pos: usize,
        k: Cont<'_>,
    ) -> bool {
        let can_more = max.is_none_or(|m| count < m);
        if count >= min && !greedy && k(pos) {
            return true;
        }
        if can_more
            && self.node(node, pos, &mut |p| {
                (p != pos || count < min) && self.repeat(node, min, max, greedy, count + 1, p, k)
            })
        {
            return true;
        }
        count >= min && greedy && k(pos)
    }
}

impl Pattern {
    pub fn compile(source: &str) -> Result<Self, String> {
        let mut parser = Parser {
            chars: source.chars().collect(),
            pos: 0,
            source,
        };
        let alts = parser.alternation()?;
        if parser.pos < parser.chars.len() {
            return parser.fail("Unmatched closing ')'");
        }
        Ok(Self {
            root: vec![Node::Alt(alts)],
        })
    }

    pub fn full_match(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        let m = Matcher { text: &chars };
        let len = chars.len();
        m.seq(&self.root, 0, &mut |p| p == len)
    }
}

#[cfg(test)]
mod tests {
    use super::Pattern;

    fn full(p: &str, s: &str) -> bool {
        Pattern::compile(p).unwrap().full_match(s)
    }

    #[test]
    fn literals_classes_and_anchors() {
        assert!(full("^\\d+$", "12345"));
        assert!(!full("^\\d+$", "12a45"));
        assert!(full("\\d+", "42"));
        assert!(!full("\\d+", "42x"));
        assert!(full("[a-zA-Z_][\\w-]*", "Abc-def_1"));
        assert!(!full("[^0-9]+", "ab3"));
        assert!(full("a.c", "abc"));
        assert!(full("[-a]+", "a-a"));
        assert!(full("\\.", "."));
    }

    #[test]
    fn groups_alternation_and_counts() {
        assert!(full("(dev|prd)-[0-9]{2,3}", "prd-123"));
        assert!(!full("(dev|prd)-[0-9]{2,3}", "prd-1234"));
        assert!(full("(?:ab)+c?", "ababab"));
        assert!(full("(a|ab)(c|bcd)(d*)", "abcd"));
        assert!(full("x{3}", "xxx"));
        assert!(full("x{2,}", "xxxxx"));
        assert!(!full("x{2,}", "x"));
        assert!(full("(a*)*b", "aaab"));
        assert!(full(".*?end", "the end"));
        assert!(full("a{,", "a{,"));
    }

    #[test]
    fn long_inputs_do_not_recurse_per_character() {
        let text = "a".repeat(8192);
        assert!(full("^[a-z]*$", &text));
        assert!(full(".+", &text));
    }

    #[test]
    fn rejects_invalid_patterns() {
        for p in ["(", "[a-", "*a", "a**", "\\q", "a)", "[z-a]", "x{3,1}"] {
            assert!(Pattern::compile(p).is_err(), "{p}");
        }
    }
}
