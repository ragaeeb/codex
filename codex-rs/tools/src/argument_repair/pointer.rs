use std::fmt;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct JsonPointer {
    tokens: Vec<String>,
}

impl JsonPointer {
    pub(crate) fn root() -> Self {
        Self { tokens: Vec::new() }
    }

    pub(crate) fn parse(pointer: &str) -> Option<Self> {
        if pointer.is_empty() {
            return Some(Self::root());
        }
        let pointer = pointer.strip_prefix('/')?;
        let tokens = pointer
            .split('/')
            .map(decode_token)
            .collect::<Option<Vec<_>>>()?;
        Some(Self { tokens })
    }

    pub(crate) fn child(&self, token: impl AsRef<str>) -> Self {
        let mut tokens = self.tokens.clone();
        tokens.push(token.as_ref().to_string());
        Self { tokens }
    }

    pub(crate) fn from_tokens(tokens: Vec<String>) -> Self {
        Self { tokens }
    }

    pub(crate) fn tokens(&self) -> &[String] {
        &self.tokens
    }

    pub(crate) fn as_string(&self) -> String {
        let mut pointer = String::new();
        for token in &self.tokens {
            pointer.push('/');
            for character in token.chars() {
                match character {
                    '~' => pointer.push_str("~0"),
                    '/' => pointer.push_str("~1"),
                    _ => pointer.push(character),
                }
            }
        }
        pointer
    }
}

impl fmt::Debug for JsonPointer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_string())
    }
}

fn decode_token(token: &str) -> Option<String> {
    let mut decoded = String::with_capacity(token.len());
    let mut characters = token.chars();
    while let Some(character) = characters.next() {
        if character != '~' {
            decoded.push(character);
            continue;
        }
        match characters.next()? {
            '0' => decoded.push('~'),
            '1' => decoded.push('/'),
            _ => return None,
        }
    }
    Some(decoded)
}
