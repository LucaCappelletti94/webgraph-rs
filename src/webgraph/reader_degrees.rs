use super::*;
use anyhow::Result;
use dsi_bitstream::prelude::*;

/// Fast iterator over the degrees of each node in the graph without having
/// the offsets.
/// This has limited uses, but is very fast. Most notably, this can be used to
/// build the offsets of a graph.
pub struct WebgraphDegreesIter<CR: WebGraphCodesReader> {
    codes_reader: CR,
    backrefs: Vec<u64>,
    node_id: u64,
    min_interval_length: usize,
    compression_window: usize,
    number_of_nodes: usize,
}

impl<CR: WebGraphCodesReader + BitSeek> WebgraphDegreesIter<CR> {
    pub fn get_position(&self) -> usize {
        self.codes_reader.get_position()
    }
}

impl<CR: WebGraphCodesReader + BitSeek> Iterator for WebgraphDegreesIter<CR> {
    type Item = (usize, u64, u64);
    fn next(&mut self) -> Option<(usize, u64, u64)> {
        if self.node_id >= self.number_of_nodes as u64 {
            return None;
        }
        let offset = self.get_position();
        Some((offset, self.node_id, self.next_degree().unwrap()))
    }
}

impl<CR: WebGraphCodesReader> WebgraphDegreesIter<CR> {
    pub fn new(
        codes_reader: CR,
        min_interval_length: usize,
        compression_window: usize,
        number_of_nodes: usize,
    ) -> Self {
        Self {
            codes_reader,
            backrefs: vec![0; compression_window + 1],
            node_id: 0,
            min_interval_length,
            compression_window,
            number_of_nodes,
        }
    }

    pub fn get_number_of_nodes(&self) -> usize {
        self.number_of_nodes
    }

    #[inline(always)]
    pub fn next_degree(&mut self) -> Result<u64> {
        let degree = self.codes_reader.read_outdegree()?;
        // no edges, we are done!
        if degree == 0 {
            self.backrefs[self.node_id as usize % self.compression_window] = degree;
            self.node_id += 1;
            return Ok(degree);
        }

        let mut nodes_left_to_decode = degree;

        // read the reference offset
        let ref_delta = self.codes_reader.read_reference_offset()?;
        // if we copy nodes from a previous one
        if ref_delta != 0 {
            // compute the node id of the reference
            let reference_node_id = self.node_id - ref_delta;
            // retrieve the data
            let ref_degree = self.backrefs[reference_node_id as usize % self.compression_window];
            // get the info on which destinations to copy
            let number_of_blocks = self.codes_reader.read_block_count()? as usize;

            // no blocks, we copy everything
            if number_of_blocks == 0 {
                nodes_left_to_decode -= ref_degree;
            } else {
                // otherwise we copy only the blocks of even index

                // the first block could be zero
                let mut idx = self.codes_reader.read_blocks()?;
                nodes_left_to_decode -= idx;

                // while the other can't
                for block_id in 1..number_of_blocks {
                    let block = self.codes_reader.read_blocks()?;
                    let end = idx + block + 1;
                    if block_id % 2 == 0 {
                        nodes_left_to_decode -= block + 1;
                    }
                    idx = end;
                }
                if number_of_blocks & 1 == 0 {
                    nodes_left_to_decode -= ref_degree - idx;
                }
            }
        };

        // if we still have to read nodes
        if nodes_left_to_decode != 0 {
            // read the number of intervals
            let number_of_intervals = self.codes_reader.read_interval_count()? as usize;
            if number_of_intervals != 0 {
                // pre-allocate with capacity for efficency
                let _ = self.codes_reader.read_interval_start()?;
                let mut delta = self.codes_reader.read_interval_len()?;
                delta += self.min_interval_length as u64;
                // save the first interval
                nodes_left_to_decode -= delta;
                // decode the intervals
                for _ in 1..number_of_intervals {
                    let _ = self.codes_reader.read_interval_start()?;
                    delta = self.codes_reader.read_interval_len()?;
                    delta += self.min_interval_length as u64;

                    nodes_left_to_decode -= delta;
                }
            }
        }

        // decode the extra nodes if needed
        if nodes_left_to_decode != 0 {
            // pre-allocate with capacity for efficency
            let _ = self.codes_reader.read_first_residual()?;
            // decode the successive extra nodes
            for _ in 1..nodes_left_to_decode {
                let _ = self.codes_reader.read_residual()?;
            }
        }

        self.backrefs[self.node_id as usize % self.compression_window] = degree;
        self.node_id += 1;
        Ok(degree)
    }
}

#[cfg(feature = "std")]
/// `std` dependent implementations for [`WebgraphDegreesIter`]
mod p {
    use super::*;
    use crate::utils::MmapBackend;
    use anyhow::{bail, Result};
    use java_properties;
    use mmap_rs::*;
    use std::fs::*;
    use std::io::*;

    type ReadType = u32;
    type BufferType = u64;

    impl<'a>
        WebgraphDegreesIter<
            DefaultCodesReader<
                M2L,
                BufferedBitStreamRead<
                    M2L,
                    BufferType,
                    MemWordReadInfinite<ReadType, MmapBackend<ReadType>>,
                >,
            >,
        >
    {
        pub fn load_mapped(basename: &str) -> Result<Self> {
            let f = File::open(format!("{}.properties", basename))?;
            let map = java_properties::read(BufReader::new(f))?;

            let compressions_flags = map.get("compressionflags").unwrap().as_str();
            if compressions_flags != "" {
                bail!("You cannot read a graph with compression_flags not empty with the default codes reader");
            }

            let mut file = std::fs::File::open(format!("{}.graph", basename)).unwrap();
            let mut file_len = file.seek(std::io::SeekFrom::End(0)).unwrap();

            // align the len to readtypes, TODO!: arithmize
            while file_len % std::mem::size_of::<ReadType>() as u64 != 0 {
                file_len += 1;
            }

            let data = unsafe {
                MmapOptions::new(file_len as _)
                    .unwrap()
                    .with_file(file, 0)
                    .map()
                    .unwrap()
            };

            let code_reader =
                DefaultCodesReader::new(BufferedBitStreamRead::<M2L, BufferType, _>::new(
                    MemWordReadInfinite::new(MmapBackend::new(data)),
                ));
            let seq_reader = WebgraphDegreesIter::new(
                code_reader,
                map.get("minintervallength").unwrap().parse::<usize>()?,
                map.get("windowsize").unwrap().parse::<usize>()?,
                map.get("nodes").unwrap().parse::<usize>()?,
            );

            Ok(seq_reader)
        }
    }
}
