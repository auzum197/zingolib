//! Contains structs for querying a database about notes.

use zcash_protocol::{PoolType, ShieldedPool};

/// Selects received notes by how they been spent
#[derive(Clone, Copy)]
pub struct OutputSpendStatusQuery {
    pub unspent: bool,
    pub pending_spent: bool,
    pub spent: bool,
}
impl OutputSpendStatusQuery {
    #[must_use]
    pub fn any() -> Self {
        Self {
            unspent: true,
            pending_spent: true,
            spent: true,
        }
    }
    #[must_use]
    pub fn only_unspent() -> Self {
        Self {
            unspent: true,
            pending_spent: false,
            spent: false,
        }
    }
    #[must_use]
    pub fn only_pending_spent() -> Self {
        Self {
            unspent: false,
            pending_spent: true,
            spent: false,
        }
    }
    #[must_use]
    pub fn only_spent() -> Self {
        Self {
            unspent: false,
            pending_spent: false,
            spent: true,
        }
    }
    #[must_use]
    pub fn spentish() -> Self {
        Self {
            unspent: false,
            pending_spent: true,
            spent: true,
        }
    }
}

/// Selects received notes by pool
#[derive(Clone, Copy)]
pub struct OutputPoolQuery {
    pub transparent: bool,
    pub sapling: bool,
    pub orchard: bool,
    pub ironwood: bool,
}
impl OutputPoolQuery {
    #[must_use]
    pub fn any() -> Self {
        Self {
            transparent: true,
            sapling: true,
            orchard: true,
            ironwood: true,
        }
    }
    #[must_use]
    pub fn shielded() -> Self {
        Self {
            transparent: false,
            sapling: true,
            orchard: true,
            ironwood: true,
        }
    }
    #[must_use]
    pub fn one_pool(pool_type: PoolType) -> Self {
        match pool_type {
            PoolType::Transparent => Self {
                transparent: true,
                sapling: false,
                orchard: false,
                ironwood: false,
            },
            PoolType::Shielded(ShieldedPool::Sapling) => Self {
                transparent: false,
                sapling: true,
                orchard: false,
                ironwood: false,
            },
            PoolType::Shielded(ShieldedPool::Orchard) => Self {
                transparent: false,
                sapling: false,
                orchard: true,
                ironwood: false,
            },
            PoolType::Shielded(ShieldedPool::Ironwood) => Self {
                transparent: false,
                sapling: false,
                orchard: false,
                ironwood: true,
            },
        }
    }
}

/// Selects received notes by any properties
#[derive(Clone, Copy)]
pub struct OutputQuery {
    /// selects spend status properties
    /// the query is expected to match note with ANY of the specified `spend_stati` AND ANY of the specified pools
    pub spend_status: OutputSpendStatusQuery,
    pub pools: OutputPoolQuery,
}

impl OutputQuery {
    #[must_use]
    pub fn any() -> Self {
        Self {
            spend_status: OutputSpendStatusQuery::any(),
            pools: OutputPoolQuery::any(),
        }
    }
    /// a query that only accepts unspent notes, from any pool.
    #[must_use]
    pub fn only_unspent() -> Self {
        Self {
            spend_status: OutputSpendStatusQuery {
                unspent: true,
                pending_spent: false,
                spent: false,
            },
            pools: OutputPoolQuery::any(),
        }
    }

    /// build a query, specifying each stipulation
    #[must_use]
    pub fn stipulations(
        unspent: bool,
        pending_spent: bool,
        spent: bool,
        transparent: bool,
        sapling: bool,
        orchard: bool,
        ironwood: bool,
    ) -> Self {
        Self {
            spend_status: OutputSpendStatusQuery {
                unspent,
                pending_spent,
                spent,
            },
            pools: OutputPoolQuery {
                transparent,
                sapling,
                orchard,
                ironwood,
            },
        }
    }
    #[must_use]
    pub fn unspent(&self) -> bool {
        self.spend_status.unspent
    }
    #[must_use]
    pub fn pending_spent(&self) -> bool {
        self.spend_status.pending_spent
    }
    #[must_use]
    pub fn spent(&self) -> bool {
        self.spend_status.spent
    }
    #[must_use]
    pub fn transparent(&self) -> bool {
        self.pools.transparent
    }
    #[must_use]
    pub fn sapling(&self) -> bool {
        self.pools.sapling
    }
    #[must_use]
    pub fn orchard(&self) -> bool {
        self.pools.orchard
    }
    #[must_use]
    pub fn ironwood(&self) -> bool {
        self.pools.ironwood
    }
}
