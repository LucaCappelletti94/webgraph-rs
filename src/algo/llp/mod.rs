/*
 * SPDX-FileCopyrightText: 2024 Tommaso Fontana
 * SPDX-FileCopyrightText: 2024 Sebastiano Vigna
 *
 * SPDX-License-Identifier: Apache-2.0 OR LGPL-2.1-or-later
 */

use crate::prelude::*;
use crate::traits::*;
use anyhow::{Context, Result};
use dsi_progress_logger::prelude::*;
use epserde::prelude::*;
use llp::preds::PredParams;
use predicates::Predicate;

use common_traits::UnsignedInt;
use log::info;
use rand::rngs::SmallRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use rayon::prelude::*;
use std::collections::HashMap;
use std::env::temp_dir;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use sux::traits::IndexedDict;
use sux::traits::Succ;

pub(crate) mod gap_cost;
pub(crate) mod label_store;
pub mod preds;

fn labels_path(gamma_index: usize) -> PathBuf {
    [temp_dir(), format!("labels_{}.bin", gamma_index).into()]
        .iter()
        .collect()
}

/// Write the permutation computed by the LLP algorithm inside `perm`,
/// and return the labels of said permutation.
///
/// # References
/// [Layered Label Propagation: A MultiResolution Coordinate-Free Ordering for Compressing Social Networks](https://arxiv.org/pdf/1011.5425.pdf>)
#[allow(clippy::type_complexity)]
#[allow(clippy::too_many_arguments)]
pub fn layered_label_propagation<'a, R: RandomAccessGraph + Sync>(
    graph: &'a R,
    deg_cumul: &(impl Succ<Input = usize, Output = usize> + Send + Sync),
    gammas: Vec<f64>,
    num_threads: Option<usize>,
    chunk_size: usize,
    granularity: Option<usize>,
    seed: u64,
    predicate: impl Predicate<preds::PredParams>,
) -> Result<Box<[usize]>> {
    let num_nodes = graph.num_nodes();

    let granularity = granularity.unwrap_or(((graph.num_arcs() >> 9) as usize).max(1024));

    // init the permutation with the indices
    let mut update_perm = (0..num_nodes).collect::<Vec<_>>();

    let mut can_change = Vec::with_capacity(num_nodes as _);
    can_change.extend((0..num_nodes).map(|_| AtomicBool::new(true)));
    let label_store = label_store::LabelStore::new(num_nodes as _);
    let stack_size = std::env::var("RUST_MIN_STACK")
        .map(|value| value.parse().unwrap())
        .unwrap_or(1024 * num_nodes.ilog2_ceil() as usize);
    // build a thread_pool so we avoid having to re-create the threads
    let num_threads = num_threads.unwrap_or_else(num_cpus::get);
    let thread_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads + 1)
        .stack_size(stack_size)
        .build()
        .context("Could not create thread pool")?;

    // init the gamma progress logger
    let mut gamma_pl = ProgressLogger::default();
    gamma_pl
        .display_memory(true)
        .item_name("gamma")
        .expected_updates(Some(gammas.len()));

    // init the iteration progress logger
    let mut iter_pl = ProgressLogger::default();
    iter_pl.item_name("update");

    // init the update progress logger
    let mut update_pl = ProgressLogger::default();
    update_pl
        .item_name("node")
        .local_speed(true)
        .expected_updates(Some(num_nodes));

    let seed = AtomicU64::new(seed);
    let mut costs = Vec::with_capacity(gammas.len());

    gamma_pl.start(format!("Running {} threads", num_threads));
    info!("Stopping criterion: {predicate}");

    for (gamma_index, gamma) in gammas.iter().enumerate() {
        // Reset mutable state for the next gamma
        iter_pl.start(format!(
            "Starting iterations with gamma={} ({}/{})...",
            gamma,
            gamma_index + 1,
            gammas.len(),
        ));
        let mut obj_func = 0.0;
        label_store.init();
        can_change
            .iter()
            .for_each(|x| x.store(true, Ordering::Relaxed));

        for update in 0.. {
            update_pl.start(format!("Starting update {}...", update));

            update_perm.iter_mut().enumerate().for_each(|(i, x)| *x = i);
            thread_pool.install(|| {
                // parallel shuffle
                update_perm.par_chunks_mut(chunk_size).for_each(|chunk| {
                    let seed = seed.fetch_add(1, Ordering::Relaxed);
                    let mut rand = SmallRng::seed_from_u64(seed);
                    chunk.shuffle(&mut rand);
                });
            });

            // If this iteration modified anything (early stop)
            let modified = AtomicUsize::new(0);

            let delta_obj_func = graph.par_apply(
                |range| {
                    let mut map = HashMap::with_capacity(1024);
                    let mut rand = SmallRng::seed_from_u64(range.start as u64);
                    let mut local_obj_func = 0.0;
                    for &node in &update_perm[range] {
                        // if the node can't change we can skip it
                        if !can_change[node].load(Ordering::Relaxed) {
                            continue;
                        }
                        // set that the node can't change by default and we'll unset later it if it can
                        can_change[node].store(false, Ordering::Relaxed);

                        let successors = graph.successors(node);
                        // TODO
                        /*if successors.len() == 0 {
                            continue;
                        }*/
                        if graph.outdegree(node) == 0 {
                            continue;
                        }

                        // get the label of this node
                        let curr_label = label_store.label(node);
                        // get the count of how many times a
                        // label appears in the successors
                        map.clear();
                        for succ in successors {
                            map.entry(label_store.label(succ))
                                .and_modify(|counter| *counter += 1)
                                .or_insert(1_usize);
                        }
                        // add the current label to the map
                        map.entry(curr_label).or_insert(0_usize);

                        let mut max = f64::NEG_INFINITY;
                        let mut old = 0.0;
                        let mut majorities = vec![];
                        // compute the most entropic label
                        for (&label, &count) in map.iter() {
                            let volume = label_store.volume(label);
                            // here there is a change from the java version as
                            // curr_label does not have -1 to its volume as
                            // it is in java, but it should be neglegible
                            let val = (1.0 + gamma) * count as f64 - gamma * (volume + 1) as f64;

                            if max == val {
                                majorities.push(label);
                            }

                            if val > max {
                                majorities.clear();
                                max = val;
                                majorities.push(label);
                            }

                            if label == curr_label {
                                old = val;
                            }
                        }
                        // randomly break ties
                        let next_label = *majorities.choose(&mut rand).unwrap();
                        // if the label changed we need to update the label store
                        // and signal that this could change the neighbour nodes
                        if next_label != curr_label {
                            modified.fetch_add(1, Ordering::Relaxed);
                            for succ in graph.successors(node) {
                                can_change[succ].store(true, Ordering::Relaxed);
                            }
                            label_store.set(node, next_label);
                        }
                        local_obj_func += max - old;
                    }
                    local_obj_func
                },
                |delta_obj_func_0, delta_obj_func_1| delta_obj_func_0 + delta_obj_func_1,
                &thread_pool,
                granularity,
                deg_cumul,
                Some(&mut update_pl),
            );

            update_pl.done_with_count(num_nodes);
            iter_pl.update_and_display();

            obj_func += delta_obj_func;
            let gain = delta_obj_func / obj_func;

            info!("Gain: {}", gain);
            info!("Modified: {}", modified.load(Ordering::Relaxed),);

            if predicate.eval(&PredParams {
                num_nodes: graph.num_nodes(),
                num_arcs: graph.num_arcs(),
                gain,
                modified: modified.load(Ordering::Relaxed),
                update,
            }) || modified.load(Ordering::Relaxed) == 0
            {
                break;
            }
        }

        iter_pl.done();

        update_perm.iter_mut().enumerate().for_each(|(i, x)| *x = i);
        // create sorted clusters by contiguous labels
        update_perm.par_sort_by(|&a, &b| label_store.label(a as _).cmp(&label_store.label(b as _)));
        invert_in_place(&mut update_perm);

        let labels =
            unsafe { std::mem::transmute::<&[AtomicUsize], &[usize]>(&label_store.labels) };

        let cost = gap_cost::compute_log_gap_cost(
            &thread_pool,
            &PermutedGraph {
                graph,
                perm: &update_perm,
            },
            deg_cumul,
            None,
        );
        info!("Log-gap cost: {}", cost);
        costs.push(cost);

        // storing the perms
        let mut file =
            std::fs::File::create(labels_path(gamma_index)).context("Could not write labels")?;
        labels
            .serialize(&mut file)
            .context("Could not serialize labels")?;

        gamma_pl.update_and_display();
    }

    gamma_pl.done();

    // compute the indices that sorts the gammas by cost
    let mut gamma_indices = (0..costs.len()).collect::<Vec<_>>();
    // sort in descending order
    gamma_indices.sort_by(|a, b| costs[*b].total_cmp(&costs[*a]));

    // the best gamma is the last because it has the min cost
    let best_gamma_index = *gamma_indices.last().unwrap();
    let worst_gamma_index = gamma_indices[0];
    let best_gamma = gammas[best_gamma_index];
    let worst_gamma = gammas[worst_gamma_index];
    info!(
        "Best gamma: {}\twith log-gap cost {}",
        best_gamma, costs[best_gamma_index]
    );
    info!(
        "Worst gamma: {}\twith log-gap cost {}",
        worst_gamma, costs[worst_gamma_index]
    );
    // reuse the update_perm to store the final permutation
    let mut temp_perm = update_perm;

    let mut result_labels = <Vec<usize>>::load_mem(labels_path(best_gamma_index))
        .context("Could not load labels from best gammar")?
        .to_vec();

    for (i, gamma_index) in gamma_indices.iter().enumerate() {
        info!("Starting step {}...", i);
        let labels =
            <Vec<usize>>::load_mem(labels_path(*gamma_index)).context("Could not load labels")?;
        combine(&mut result_labels, *labels, &mut temp_perm).context("Could not combine labels")?;
        // This recombination with the best labels does not appear in the paper, but
        // it is not harmful and fixes a few corner cases in which experimentally
        // LLP does not perform well. It was introduced by Marco Rosa in the Java
        // LAW code.
        let best_labels = <Vec<usize>>::load_mem(labels_path(best_gamma_index))
            .context("Could not load labels from best gamma")?;
        let number_of_labels = combine(&mut result_labels, *best_labels, &mut temp_perm)?;
        info!("Number of labels: {}", number_of_labels);
        info!("Finished step {}.", i);
    }

    Ok(result_labels.into_boxed_slice())
}

/// combine the labels from two permutations into a single one
fn combine(result: &mut [usize], labels: &[usize], temp_perm: &mut [usize]) -> Result<usize> {
    // re-init the permutation
    temp_perm.iter_mut().enumerate().for_each(|(i, x)| *x = i);
    // permute by the devilish function
    temp_perm.par_sort_by(|&a, &b| {
        (result[labels[a]].cmp(&result[labels[b]]))
            .then_with(|| labels[a].cmp(&labels[b]))
            .then_with(|| result[a].cmp(&result[b]))
    });
    let mut prev_labels = (result[temp_perm[0]], labels[temp_perm[0]]);
    let mut curr_label = 0;
    temp_perm[0] = curr_label;

    for i in 1..temp_perm.len() {
        let curr_labels = (result[temp_perm[i]], labels[temp_perm[i]]);
        if prev_labels != curr_labels {
            curr_label += 1;
            prev_labels = curr_labels
        }
        result[temp_perm[i]] = curr_label;
    }

    Ok(curr_label + 1)
}

/// Invert the given permutation in place.
pub fn invert_in_place(perm: &mut [usize]) {
    for n in 0..perm.len() {
        let mut i = perm[n];
        if (i as isize) < 0 {
            perm[n] = !i;
        } else if i != n {
            let mut k = n;
            loop {
                let j = perm[i];
                perm[i] = !k;
                if j == n {
                    perm[n] = i;
                    break;
                }
                k = i;
                i = j;
            }
        }
    }
}

#[cfg(test)]
#[test]
fn test_invert_in_place() {
    use rand::prelude::SliceRandom;
    let mut v = (0..1000).collect::<Vec<_>>();
    v.shuffle(&mut rand::thread_rng());
    let mut w = v.clone();
    invert_in_place(&mut w);
    for i in 0..v.len() {
        assert_eq!(w[v[i]], i);
    }
}