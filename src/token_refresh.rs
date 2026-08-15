use arc_swap::ArcSwap;
use ethers::types::Address;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct TokenList {
    inner: Arc<TokenListInner>,
}

#[derive(Debug)]
struct TokenListInner {
    list: ArcSwap<Vec<Address>>,
    set: ArcSwap<HashSet<Address>>,
}

impl TokenList {
    pub fn new(tokens: Vec<Address>) -> Self {
        let set: HashSet<Address> = tokens.iter().copied().collect();
        let inner = TokenListInner {
            list: ArcSwap::from_pointee(tokens),
            set: ArcSwap::from_pointee(set),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    pub fn current(&self) -> Arc<Vec<Address>> {
        self.inner.list.load_full()
    }

    pub fn current_set(&self) -> Arc<HashSet<Address>> {
        self.inner.set.load_full()
    }
}
