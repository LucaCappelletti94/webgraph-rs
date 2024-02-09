/*
 * SPDX-FileCopyrightText: 2023 Inria
 * SPDX-FileCopyrightText: 2023 Tommaso Fontana
 *
 * SPDX-License-Identifier: Apache-2.0 OR LGPL-2.1-or-later
 */

use crate::prelude::*;
use anyhow::Result;
use clap::{ArgMatches, Args, Command, FromArgMatches};
use dsi_bitstream::prelude::*;
use itertools::Itertools;
use lender::*;
use rand::rngs::SmallRng;
use rand::Rng;
use rand::SeedableRng;
use std::hint::black_box;

pub const COMMAND_NAME: &str = "bvgraph";

#[derive(Args, Debug)]
#[command(about = "Benchmarks the Rust BVGraph implementation.", long_about = None)]
struct CliArgs {
    /// The basename of the graph.
    basename: String,

    /// Perform a random-access test on this number of randomly chosen nodes.
    #[arg(short, long)]
    random: Option<usize>,

    /// The number of repeats.
    #[arg(short = 'R', long, default_value = "10")]
    repeats: usize,

    /// In random-access test, test just access to the first successor.
    #[arg(short = 'f', long)]
    first: bool,

    /// Static dispatch for speed tests (default BVGraph parameters).
    #[arg(short = 's', long = "static")]
    _static: bool,

    /// Test sequential high-speed offset/degree scanning.
    #[arg(short = 'd', long)]
    degrees: bool,

    /// Do not test speed, but check that the sequential and random-access successor lists are the same.
    #[arg(short = 'c', long)]
    check: bool,
}

pub fn cli(command: Command) -> Command {
    command.subcommand(CliArgs::augment_args(Command::new(COMMAND_NAME)))
}

pub fn main(submatches: &ArgMatches) -> Result<()> {
    let args = CliArgs::from_arg_matches(submatches)?;

    match get_endianess(&args.basename)?.as_str() {
        #[cfg(any(
            feature = "be_bins",
            not(any(feature = "be_bins", feature = "le_bins"))
        ))]
        BE::NAME => match args._static {
            true => bench_webgraph::<BE, Static>(args),
            false => bench_webgraph::<BE, Dynamic>(args),
        },
        #[cfg(any(
            feature = "le_bins",
            not(any(feature = "be_bins", feature = "le_bins"))
        ))]
        LE::NAME => match args._static {
            true => bench_webgraph::<LE, Static>(args),
            false => bench_webgraph::<LE, Dynamic>(args),
        },
        e => panic!("Unknown endianness: {}", e),
    }
}

fn bench_random(graph: impl RandomAccessGraph, samples: usize, repeats: usize, first: bool) {
    // Random-access speed test
    for _ in 0..repeats {
        let mut rng = SmallRng::seed_from_u64(0);
        let mut c: u64 = 0;
        let num_nodes = graph.num_nodes();
        let start = std::time::Instant::now();
        if first {
            for _ in 0..samples {
                black_box(
                    graph
                        .successors(rng.gen_range(0..num_nodes))
                        .into_iter()
                        .next()
                        .unwrap_or(0),
                );
            }
        } else {
            for _ in 0..samples {
                c += black_box(
                    graph
                        .successors(rng.gen_range(0..num_nodes))
                        .into_iter()
                        .count() as u64,
                );
            }
        }

        println!(
            "{}:    {:>20} ns/arc",
            if first { "First" } else { "Random" },
            (start.elapsed().as_secs_f64() / c as f64) * 1e9
        );
    }
}

fn bench_seq(graph: impl SequentialGraph, repeats: usize) {
    for _ in 0..repeats {
        let mut c: u64 = 0;

        let start = std::time::Instant::now();
        let mut iter = graph.iter();
        while let Some((_, succ)) = iter.next() {
            c += succ.into_iter().count() as u64;
        }
        println!(
            "Sequential:{:>20} ns/arc",
            (start.elapsed().as_secs_f64() / c as f64) * 1e9
        );

        assert_eq!(c, graph.num_arcs_hint().unwrap());
    }
}

fn bench_webgraph<E: Endianness, D: Dispatch>(args: CliArgs) -> Result<()>
where
    for<'a> BufBitReader<E, MemWordReader<u32, &'a [u32]>>: CodeRead<E> + BitSeek,
{
    if args.check {
        let graph = BVGraph::with_basename(&args.basename)
            .endianness::<E>()
            .load()?;

        let seq_graph = BVGraphSeq::with_basename(&args.basename)
            .endianness::<E>()
            .load()?;

        let mut deg_reader = seq_graph.offset_deg_iter();

        // Check that sequential and random-access interfaces return the same result
        for_![ (node, seq_succ) in seq_graph {
            let succ = graph.successors(node);

            assert_eq!(deg_reader.next_degree()?, seq_succ.len());
            assert_eq!(succ.collect_vec(), seq_succ.collect_vec());
        }];
    } else if args.degrees {
        let seq_graph = BVGraphSeq::with_basename(&args.basename)
            .endianness::<E>()
            .load()?;

        for _ in 0..args.repeats {
            let mut deg_reader = seq_graph.offset_deg_iter();

            let mut c: u64 = 0;
            let start = std::time::Instant::now();
            for _ in 0..seq_graph.num_nodes() {
                c += black_box(deg_reader.next_degree()? as u64);
            }
            println!(
                "Degrees Only:{:>20} ns/arc",
                (start.elapsed().as_secs_f64() / c as f64) * 1e9
            );

            assert_eq!(c, seq_graph.num_arcs_hint().unwrap());
        }
    } else {
        match (
            args.random,
            std::any::TypeId::of::<D>() == std::any::TypeId::of::<Dynamic>(),
        ) {
            (Some(samples), true) => {
                bench_random(
                    BVGraph::with_basename(&args.basename)
                        .endianness::<E>()
                        .dispatch::<Dynamic>()
                        .mode::<Mmap>()
                        .flags(MemoryFlags::TRANSPARENT_HUGE_PAGES | MemoryFlags::RANDOM_ACCESS)
                        .load()?,
                    samples,
                    args.repeats,
                    args.first,
                );
            }
            (Some(samples), false) => {
                bench_random(
                    BVGraph::with_basename(&args.basename)
                        .endianness::<E>()
                        .dispatch::<Static>()
                        .mode::<Mmap>()
                        .flags(MemoryFlags::TRANSPARENT_HUGE_PAGES | MemoryFlags::RANDOM_ACCESS)
                        .load()?,
                    samples,
                    args.repeats,
                    args.first,
                );
            }
            (None, true) => {
                bench_seq(
                    BVGraphSeq::with_basename(&args.basename)
                        .endianness::<E>()
                        .dispatch::<Dynamic>()
                        .mode::<Mmap>()
                        .flags(MemoryFlags::TRANSPARENT_HUGE_PAGES | MemoryFlags::SEQUENTIAL)
                        .load()?,
                    args.repeats,
                );
            }
            (None, false) => {
                bench_seq(
                    BVGraphSeq::with_basename(&args.basename)
                        .endianness::<E>()
                        .dispatch::<Static>()
                        .mode::<Mmap>()
                        .flags(MemoryFlags::TRANSPARENT_HUGE_PAGES | MemoryFlags::SEQUENTIAL)
                        .load()?,
                    args.repeats,
                );
            }
        }
    }
    Ok(())
}
