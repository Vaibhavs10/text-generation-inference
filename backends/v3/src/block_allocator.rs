use std::{cmp::min, collections::HashMap, sync::Arc};
use tokio::sync::{mpsc, oneshot};

use crate::{radix::NodeId, RadixTrie};

#[derive(Debug, Clone)]
pub(crate) struct BlockAllocation {
    pub blocks: Vec<u32>,
    pub slots: Vec<u32>,

    /// Prefix that was cached and for which the KV does not have to
    /// be recomputed.
    pub prefix_len: u32,

    pub allocation_id: u64,

    block_allocator: BlockAllocator,
}

impl Drop for BlockAllocation {
    fn drop(&mut self) {
        self.block_allocator
            .free(self.blocks.clone(), self.allocation_id)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BlockAllocator {
    /// Channel to communicate with the background task
    block_allocator: mpsc::UnboundedSender<BlockAllocatorCommand>,
}

impl BlockAllocator {
    pub(crate) fn new(
        max_batch_total_tokens: u32,
        block_size: u32,
        window_size: Option<u32>,
    ) -> Self {
        // Create channel
        let (sender, receiver) = mpsc::unbounded_channel();

        // Launch background queue task
        tokio::spawn(block_allocator_task(
            max_batch_total_tokens / block_size,
            block_size,
            window_size,
            receiver,
        ));

        Self {
            block_allocator: sender,
        }
    }

    pub(crate) async fn allocate(
        &self,
        tokens: u32,
        prefill_tokens: Option<Arc<Vec<u32>>>,
    ) -> Option<BlockAllocation> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.block_allocator
            .send(BlockAllocatorCommand::Allocate {
                tokens,
                prefill_tokens,
                response_sender,
            })
            .unwrap();

        response_receiver
            .await
            .unwrap()
            .map(
                |(blocks, slots, prefix_len, allocation_id)| BlockAllocation {
                    blocks,
                    slots,
                    prefix_len,
                    allocation_id,
                    block_allocator: self.clone(),
                },
            )
    }

    pub(crate) fn free(&self, blocks: Vec<u32>, allocation_id: u64) {
        self.block_allocator
            .send(BlockAllocatorCommand::Free {
                allocation_id,
                blocks,
            })
            .unwrap();
    }
}

async fn block_allocator_task(
    blocks: u32,
    block_size: u32,
    window_size: Option<u32>,
    mut receiver: mpsc::UnboundedReceiver<BlockAllocatorCommand>,
) {
    let mut allocator: Box<dyn Allocator + Send> = if block_size == 1 {
        Box::new(RadixAllocator::new(block_size, blocks, window_size))
    } else {
        Box::new(SimpleAllocator::new(blocks, block_size, window_size))
    };
    while let Some(cmd) = receiver.recv().await {
        match cmd {
            BlockAllocatorCommand::Free {
                blocks,
                allocation_id,
            } => allocator.free(blocks, allocation_id),
            BlockAllocatorCommand::Allocate {
                tokens,
                prefill_tokens,
                response_sender,
            } => {
                response_sender
                    .send(allocator.allocate(tokens, prefill_tokens))
                    .unwrap();
            }
        }
    }
}

#[derive(Debug)]
enum BlockAllocatorCommand {
    Free {
        blocks: Vec<u32>,
        allocation_id: u64,
    },
    Allocate {
        tokens: u32,
        prefill_tokens: Option<Arc<Vec<u32>>>,
        response_sender: oneshot::Sender<Option<(Vec<u32>, Vec<u32>, u32, u64)>>,
    },
}

pub trait Allocator {
    fn allocate(
        &mut self,
        tokens: u32,
        prefill_tokens: Option<Arc<Vec<u32>>>,
    ) -> Option<(Vec<u32>, Vec<u32>, u32, u64)>;

    fn free(&mut self, blocks: Vec<u32>, allocation_id: u64);
}

pub struct SimpleAllocator {
    free_blocks: Vec<u32>,
    block_size: u32,
    window_size: Option<u32>,
}

impl SimpleAllocator {
    fn new(blocks: u32, block_size: u32, window_size: Option<u32>) -> Self {
        SimpleAllocator {
            block_size,
            // Block 0 is reserved for health checks
            free_blocks: (1..blocks).collect(),
            window_size,
        }
    }
}

impl Allocator for SimpleAllocator {
    fn allocate(
        &mut self,
        tokens: u32,
        _prefill_tokens: Option<Arc<Vec<u32>>>,
    ) -> Option<(Vec<u32>, Vec<u32>, u32, u64)> {
        // Apply window size
        let (required_blocks, repeats) = {
            let (tokens, repeats) = match self.window_size {
                None => (tokens, 1),
                Some(window_size) => {
                    let repeats = (tokens + window_size - 1) / window_size;
                    let tokens = min(tokens, window_size);
                    (tokens, repeats as usize)
                }
            };
            // Pad to a multiple of block size
            let required_blocks = (tokens + self.block_size - 1) / self.block_size;
            (required_blocks, repeats)
        };

        let tokens = tokens as usize;
        if required_blocks > self.free_blocks.len() as u32 {
            None
        } else {
            let blocks = self
                .free_blocks
                .split_off(self.free_blocks.len() - required_blocks as usize);
            let mut slots =
                Vec::with_capacity((required_blocks * self.block_size * repeats as u32) as usize);

            'slots: for block_id in blocks.repeat(repeats).iter() {
                for s in (block_id * self.block_size)..((block_id + 1) * self.block_size) {
                    slots.push(s);
                    if slots.len() == tokens {
                        break 'slots;
                    }
                }
            }
            Some((blocks, slots, 0, 0))
        }
    }

    fn free(&mut self, blocks: Vec<u32>, _allocation_id: u64) {
        self.free_blocks.extend(blocks)
    }
}

struct RadixAllocator {
    allocation_id: u64,

    allocations: HashMap<u64, RadixAllocation>,

    cache_blocks: RadixTrie,

    /// Blocks that are immediately available for allocation.
    free_blocks: Vec<u32>,
}

impl RadixAllocator {
    pub fn new(block_size: u32, n_blocks: u32, window_size: Option<u32>) -> Self {
        assert_eq!(
            block_size, 1,
            "Radix tree allocator only works with block_size=1, was: {}",
            block_size
        );
        if window_size.is_some() {
            unimplemented!("Window size not supported in the prefix-caching block allocator yet");
        }

        RadixAllocator {
            allocation_id: 0,
            allocations: HashMap::new(),
            cache_blocks: RadixTrie::new(),

            // Block 0 is reserved for health checks.
            free_blocks: (1..n_blocks).collect(),
        }
    }

    fn alloc_or_reclaim(&mut self, n_blocks_needed: usize) -> Option<Vec<u32>> {
        if self.free_blocks.len() < n_blocks_needed {
            // This is a bit annoying, we first extend the free list and then
            // split it off again below. This is because we need to put it on
            // the free list if we cannot allocate enough blocks. This is only
            // temporary, the trie needs to be able to report whether it can
            // allocate the requested amount. Just not implemented yet.
            self.free_blocks.extend(
                self.cache_blocks
                    .evict(n_blocks_needed - self.free_blocks.len()),
            );
        }

        if self.free_blocks.len() >= n_blocks_needed {
            Some(
                self.free_blocks
                    .split_off(self.free_blocks.len() - n_blocks_needed),
            )
        } else {
            None
        }
    }
}

impl Allocator for RadixAllocator {
    fn allocate(
        &mut self,
        tokens: u32,
        prefill_tokens: Option<Arc<Vec<u32>>>,
    ) -> Option<(Vec<u32>, Vec<u32>, u32, u64)> {
        let mut blocks = vec![];
        let prefix_node = if let Some(prefill_tokens) = prefill_tokens.as_ref() {
            let node_id = self
                .cache_blocks
                .find(prefill_tokens.as_slice(), &mut blocks);
            // Even if this allocation fails below, we need to increase he
            // refcount to ensure that the prefix that was found is not evicted.

            node_id
        } else {
            self.cache_blocks.root_id()
        };

        self.cache_blocks.incref(prefix_node);

        let prefix_len = blocks.len();
        let suffix_len = tokens - prefix_len as u32;

        match self.alloc_or_reclaim(suffix_len as usize) {
            Some(suffix_blocks) => blocks.extend(suffix_blocks),
            None => {
                self.cache_blocks.decref(prefix_node);
                return None;
            }
        }

        // 1:1 mapping of blocks and slots.
        let slots = blocks.clone();

        let allocation = RadixAllocation {
            prefix_node,
            cached_prefix_len: prefix_len,
            prefill_tokens: prefill_tokens.clone(),
        };

        self.allocation_id += 1;
        self.allocations.insert(self.allocation_id, allocation);

        Some((blocks, slots, prefix_len as u32, self.allocation_id))
    }

    fn free(&mut self, blocks: Vec<u32>, allocation_id: u64) {
        let allocation = match self.allocations.remove(&allocation_id) {
            Some(allocation) => allocation,
            None => unreachable!("Tried to free an unknown allocation."),
        };

        self.cache_blocks.decref(allocation.prefix_node);

        if let Some(prefill_tokens) = allocation.prefill_tokens {
            let prefill_tokens = prefill_tokens.as_slice();

            // If there are prefill tokens that did not come from the cache,
            // add them to the cache.
            if prefill_tokens.len() > allocation.cached_prefix_len {
                let prefix_len = self
                    .cache_blocks
                    .insert(prefill_tokens, &blocks[..prefill_tokens.len()]);

                // We can have a prefill with the following structure:
                //
                // |---| From the prefix cache.
                // A B C D E F G
                //|--------| Found in the trie during insertion.
                //
                // This means that while processing this request there was a
                // partially overlapping request that had A..=E in its
                // prefill. In this case we need to free the blocks D E.
                self.free_blocks
                    .extend(&blocks[allocation.cached_prefix_len..prefix_len]);
            }

            // Free non-prefill blocks.
            self.free_blocks.extend(&blocks[prefill_tokens.len()..]);
        } else {
            self.free_blocks.extend(blocks);
        }
    }
}

struct RadixAllocation {
    prefix_node: NodeId,
    cached_prefix_len: usize,
    prefill_tokens: Option<Arc<Vec<u32>>>,
}

#[cfg(test)]
mod tests {
    use std::{rc::Rc, sync::Arc};

    use super::{Allocator, RadixAllocator};

    #[test]
    fn test_prefix_cache() {
        let mut cache = RadixAllocator::new(1, 12, None);
        let allocation = cache.allocate(8, Some(Arc::new(vec![0, 1, 2, 3]))).unwrap();
        assert_eq!(allocation.0, vec![4, 5, 6, 7, 8, 9, 10, 11]);
        assert_eq!(allocation.1, allocation.0);
        assert_eq!(allocation.2, 0);
        cache.free(allocation.0, allocation.3);

        let allocation = cache.allocate(8, Some(Arc::new(vec![0, 1, 2, 3]))).unwrap();
        assert_eq!(allocation.0, vec![4, 5, 6, 7, 8, 9, 10, 11]);
        assert_eq!(allocation.2, 4);
    }

    #[test]
    fn test_older_prefixes_are_collected_first() {
        let mut cache = RadixAllocator::new(1, 7, None);
        let allocation1 = cache.allocate(4, Some(Arc::new(vec![0, 1, 2, 3]))).unwrap();
        assert_eq!(allocation1.0, vec![3, 4, 5, 6]);
        assert_eq!(allocation1.2, 0);

        let allocation2 = cache.allocate(2, Some(Arc::new(vec![4, 5]))).unwrap();
        assert_eq!(allocation2.0, vec![1, 2]);
        assert_eq!(allocation2.2, 0);

        cache.free(allocation1.0, allocation1.3);
        cache.free(allocation2.0, allocation2.3);

        // We should get the blocks of the first allocation, since they are more recent.
        let allocation3 = cache.allocate(4, Some(Arc::new(vec![6, 7, 8, 9]))).unwrap();
        assert_eq!(allocation3.0, vec![3, 4, 5, 6]);
        assert_eq!(allocation3.2, 0);
    }

    #[test]
    fn correctly_free_fully_overlapping_prefills() {
        let mut cache = RadixAllocator::new(1, 10, None);
        let allocation1 = cache.allocate(4, Some(Arc::new(vec![0, 1, 2, 3]))).unwrap();
        let allocation2 = cache.allocate(4, Some(Arc::new(vec![0, 1, 2, 3]))).unwrap();

        cache.free(allocation2.0, allocation2.3);
        cache.free(allocation1.0, allocation1.3);

        let allocation3 = cache.allocate(4, Some(Arc::new(vec![0, 1, 2, 3]))).unwrap();
        assert_eq!(allocation3.2, 4);

        // 10 blocks, of which 1 reserved for health checks, 4 for the cached blocks.
        assert_eq!(cache.free_blocks.len(), 5);
    }

    #[test]
    fn correctly_free_partially_overlapping_prefills() {
        let mut cache = RadixAllocator::new(1, 20, None);
        let allocation1 = cache.allocate(4, Some(Arc::new(vec![0, 1]))).unwrap();
        assert_eq!(allocation1.0, vec![16, 17, 18, 19]);
        assert_eq!(allocation1.2, 0);

        cache.free(allocation1.0, allocation1.3);

        let allocation2 = cache
            .allocate(8, Some(Arc::new(vec![0, 1, 2, 3, 4, 5])))
            .unwrap();
        assert_eq!(allocation2.0, vec![16, 17, 12, 13, 14, 15, 18, 19]);
        assert_eq!(allocation2.2, 2);

        let allocation3 = cache
            .allocate(8, Some(Arc::new(vec![0, 1, 2, 3, 6, 7])))
            .unwrap();
        assert_eq!(allocation3.0, vec![16, 17, 6, 7, 8, 9, 10, 11]);
        assert_eq!(allocation3.2, 2);

        cache.free(allocation3.0, allocation3.3);
        cache.free(allocation2.0, allocation2.3);

        // 20 blocks, of which 1 reserved for health checks, 6 for allocation3, 2 for allocation2.
        assert_eq!(cache.free_blocks.len(), 11);

        let allocation4 = cache
            .allocate(6, Some(Arc::new(vec![0, 1, 2, 3, 4, 5])))
            .unwrap();
        assert_eq!(allocation4.0, vec![16, 17, 6, 7, 14, 15]);
        assert_eq!(allocation4.2, 6);
        assert_eq!(cache.free_blocks.len(), 11);

        let allocation5 = cache
            .allocate(6, Some(Arc::new(vec![0, 1, 2, 3, 6, 7])))
            .unwrap();
        assert_eq!(allocation5.0, vec![16, 17, 6, 7, 8, 9]);
        assert_eq!(allocation5.2, 6);
        assert_eq!(cache.free_blocks.len(), 11);
    }
}
