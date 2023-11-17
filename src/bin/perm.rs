/*
 * SPDX-FileCopyrightText: 2023 Inria
 * SPDX-FileCopyrightText: 2023 Sebastiano Vigna
 *
 * SPDX-License-Identifier: Apache-2.0 OR LGPL-2.1-or-later
 */

use anyhow::Result;
use clap::Parser;
use dsi_progress_logger::*;
use epserde::prelude::*;
use lender::*;
use std::io::{BufReader, Read};
use webgraph::graph::arc_list_graph;
use webgraph::prelude::*;

#[derive(Parser, Debug)]
#[command(about = "Permutes a graph", long_about = None)]
struct Args {
    /// The basename of the source graph.
    source: String,
    /// The basename of the destination graph.
    dest: String,
    /// The permutation.
    perm: String,

    #[arg(short = 'e', long, default_value_t = false)]
    /// Load the permutation from ε-serde format.
    epserde: bool,

    #[arg(short = 'o', long, default_value_t = false)]
    /// Build the offsets while compressing the graph .
    build_offsets: bool,

    #[clap(flatten)]
    num_cpus: NumCpusArg,

    #[clap(flatten)]
    pa: PermutationArgs,

    #[clap(flatten)]
    ca: CompressArgs,
}

fn permute(
    args: Args,
    graph: &impl SequentialGraph,
    perm: &[usize],
    num_nodes: usize,
) -> Result<()> {
    // create a stream where to dump the sorted pairs
    let mut sort_pairs = SortPairs::new(args.pa.batch_size, temp_dir(&args.pa.temp_dir)).unwrap();

    // dump the paris
    PermutedGraph { graph, perm }.iter().for_each(|(x, succ)| {
        succ.into_iter().for_each(|s| {
            sort_pairs.push(x, s).unwrap();
        })
    });
    // get a graph on the sorted data
    let edges = sort_pairs.iter()?.map(|(src, dst, _)| (src, dst));
    let g = arc_list_graph::ArcListGraph::new(num_nodes, edges);
    // compress it
    parallel_compress_sequential_iter::<
        &arc_list_graph::ArcListGraph<std::iter::Map<KMergeIters<_>, _>>,
        _,
    >(
        args.dest,
        &g,
        g.num_nodes(),
        args.ca.into(),
        args.num_cpus.num_cpus,
        temp_dir(args.pa.temp_dir),
    )?;

    Ok(())
}

pub fn main() -> Result<()> {
    let args = Args::parse();

    stderrlog::new()
        .verbosity(2)
        .timestamp(stderrlog::Timestamp::Second)
        .init()
        .unwrap();

    let mut glob_pl = ProgressLogger::default();
    glob_pl.display_memory(true).item_name("node");
    glob_pl.start("Permuting the graph...");
    // TODO!: check that batchsize fits in memory, and that print the maximum
    // batch_size usable

    let graph = webgraph::graph::bvgraph::load_seq(&args.source)?;

    let num_nodes = graph.num_nodes();
    // read the permutation

    if args.epserde {
        let perm = <Vec<usize>>::mmap(&args.perm, Flags::default())?;
        permute(args, &graph, perm.as_ref(), num_nodes)?;
    } else {
        let mut file = BufReader::new(std::fs::File::open(&args.perm)?);
        let mut perm = Vec::with_capacity(num_nodes);
        let mut buf = [0; core::mem::size_of::<usize>()];

        let mut perm_pl = ProgressLogger::default();
        perm_pl.display_memory(true).item_name("node");
        perm_pl.start("Reading the permutation...");

        for _ in 0..num_nodes {
            file.read_exact(&mut buf)?;
            perm.push(usize::from_be_bytes(buf));
            perm_pl.light_update();
        }
        perm_pl.done();
        permute(args, &graph, perm.as_ref(), num_nodes)?;
    }
    glob_pl.done();
    Ok(())
}
