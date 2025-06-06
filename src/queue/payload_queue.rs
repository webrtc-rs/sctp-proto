use crate::chunk::chunk_payload_data::ChunkPayloadData;
use crate::chunk::chunk_selective_ack::GapAckBlock;
use crate::util::*;

use std::collections::HashMap;

#[derive(Default, Debug)]
pub(crate) struct PayloadQueue {
    // length: usize,
    chunk_map: HashMap<u32, ChunkPayloadData>,
    pub(crate) sorted: Vec<u32>,
    dup_tsn: Vec<u32>,
    n_bytes: usize,
}

impl PayloadQueue {
    pub(crate) fn new() -> Self {
        PayloadQueue::default()
    }

    pub(crate) fn update_sorted_keys(&mut self) {
        self.sorted.sort_by(|a, b| {
            if sna32lt(*a, *b) {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        });
    }

    pub(crate) fn can_push(&self, p: &ChunkPayloadData, cumulative_tsn: u32) -> bool {
        !(self.chunk_map.contains_key(&p.tsn) || sna32lte(p.tsn, cumulative_tsn))
    }

    pub(crate) fn push_no_check(&mut self, p: ChunkPayloadData) {
        self.n_bytes += p.user_data.len();
        self.sorted.push(p.tsn);
        self.chunk_map.insert(p.tsn, p);
        //self.length += 1;
        self.update_sorted_keys();
    }

    /// push pushes a payload data. If the payload data is already in our queue or
    /// older than our cumulative_tsn marker, it will be recored as duplications,
    /// which can later be retrieved using popDuplicates.
    pub(crate) fn push(&mut self, p: ChunkPayloadData, cumulative_tsn: u32) -> bool {
        let ok = self.chunk_map.contains_key(&p.tsn);
        if ok || sna32lte(p.tsn, cumulative_tsn) {
            // Found the packet, log in dups
            self.dup_tsn.push(p.tsn);
            return false;
        }

        self.n_bytes += p.user_data.len();
        self.sorted.push(p.tsn);
        self.chunk_map.insert(p.tsn, p);
        //self.length += 1;
        self.update_sorted_keys();

        true
    }

    /// pop pops only if the oldest chunk's TSN matches the given TSN.
    pub(crate) fn pop(&mut self, tsn: u32) -> Option<ChunkPayloadData> {
        if !self.sorted.is_empty() && tsn == self.sorted[0] {
            self.sorted.remove(0);
            if let Some(c) = self.chunk_map.remove(&tsn) {
                //self.length -= 1;
                self.n_bytes -= c.user_data.len();
                return Some(c);
            }
        }

        None
    }

    /// get returns reference to chunkPayloadData with the given TSN value.
    pub(crate) fn get(&self, tsn: u32) -> Option<&ChunkPayloadData> {
        self.chunk_map.get(&tsn)
    }
    pub(crate) fn get_mut(&mut self, tsn: u32) -> Option<&mut ChunkPayloadData> {
        self.chunk_map.get_mut(&tsn)
    }

    /// popDuplicates returns an array of TSN values that were found duplicate.
    pub(crate) fn pop_duplicates(&mut self) -> Vec<u32> {
        self.dup_tsn.drain(..).collect()
    }

    pub(crate) fn get_gap_ack_blocks(&self, cumulative_tsn: u32) -> Vec<GapAckBlock> {
        if self.chunk_map.is_empty() {
            return vec![];
        }

        let mut b = GapAckBlock::default();
        let mut gap_ack_blocks = vec![];
        for (i, tsn) in self.sorted.iter().enumerate() {
            let diff = if *tsn >= cumulative_tsn {
                (*tsn - cumulative_tsn) as u16
            } else {
                0
            };

            if i == 0 {
                b.start = diff;
                b.end = b.start;
            } else if b.end + 1 == diff {
                b.end += 1;
            } else {
                gap_ack_blocks.push(b);

                b.start = diff;
                b.end = diff;
            }
        }

        gap_ack_blocks.push(b);

        gap_ack_blocks
    }

    pub(crate) fn get_gap_ack_blocks_string(&self, cumulative_tsn: u32) -> String {
        let mut s = format!("cumTSN={}", cumulative_tsn);
        for b in self.get_gap_ack_blocks(cumulative_tsn) {
            s += format!(",{}-{}", b.start, b.end).as_str();
        }
        s
    }

    pub(crate) fn mark_as_acked(&mut self, tsn: u32) -> usize {
        if let Some(c) = self.chunk_map.get_mut(&tsn) {
            c.acked = true;
            c.retransmit = false;
            let n = c.user_data.len();
            self.n_bytes -= n;
            c.user_data.clear();
            n
        } else {
            0
        }
    }

    pub(crate) fn get_last_tsn_received(&self) -> Option<&u32> {
        self.sorted.last()
    }

    pub(crate) fn mark_all_to_retrasmit(&mut self) {
        for c in self.chunk_map.values_mut() {
            if c.acked || c.abandoned() {
                continue;
            }
            c.retransmit = true;
        }
    }

    pub(crate) fn get_num_bytes(&self) -> usize {
        self.n_bytes
    }

    pub(crate) fn len(&self) -> usize {
        //assert_eq!(self.chunk_map.len(), self.length);
        self.chunk_map.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
