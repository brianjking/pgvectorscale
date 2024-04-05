use std::time::Instant;

use pgrx::*;

use crate::access_method::disk_index_graph::DiskIndexGraph;
use crate::access_method::graph::Graph;
use crate::access_method::graph::InsertStats;
use crate::access_method::graph::VectorProvider;
use crate::access_method::model::PgVector;
use crate::access_method::options::TSVIndexOptions;
use crate::util::page;
use crate::util::tape::Tape;
use crate::util::*;

use super::builder_graph::BuilderGraph;
use super::meta_page::MetaPage;
use super::model::{self};
use super::quantizer::Quantizer;

struct BuildState<'a, 'b> {
    memcxt: PgMemoryContexts,
    meta_page: MetaPage,
    ntuples: usize,
    tape: Tape<'a>, //The tape is a memory abstraction over Postgres pages for writing data.
    node_builder: BuilderGraph<'b>,
    started: Instant,
    stats: InsertStats,
    quantizer: Quantizer,
}

impl<'a, 'b> BuildState<'a, 'b> {
    fn new(
        index_relation: &'a PgRelation,
        meta_page: MetaPage,
        bg: BuilderGraph<'b>,
        mut quantizer: Quantizer,
    ) -> Self {
        let tape = unsafe { Tape::new(index_relation, page::PageType::Node) };

        match &mut quantizer {
            Quantizer::None => {}
            Quantizer::PQ(pq) => {
                pq.start_training(&meta_page);
            }
            Quantizer::BQ(bq) => {
                bq.start_training(&meta_page);
            }
        }
        //TODO: some ways to get rid of meta_page.clone?
        BuildState {
            memcxt: PgMemoryContexts::new("tsv build context"),
            ntuples: 0,
            meta_page: meta_page.clone(),
            tape,
            node_builder: bg,
            started: Instant::now(),
            stats: InsertStats::new(),
            quantizer: quantizer,
        }
    }
}

#[pg_guard]
pub extern "C" fn ambuild(
    heaprel: pg_sys::Relation,
    indexrel: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    let heap_relation = unsafe { PgRelation::from_pg(heaprel) };
    let index_relation = unsafe { PgRelation::from_pg(indexrel) };
    let opt = TSVIndexOptions::from_relation(&index_relation);

    notice!(
        "Starting index build. num_neighbors={} search_list_size={}, max_alpha={}, use_pq={}, pq_vector_length={}",
        opt.num_neighbors,
        opt.search_list_size,
        opt.max_alpha,
        opt.use_pq,
        opt.pq_vector_length
    );

    let dimensions = index_relation.tuple_desc().get(0).unwrap().atttypmod;
    // PQ is only applicable to high dimension vectors.
    if opt.use_pq {
        if dimensions < opt.pq_vector_length as i32 {
            error!("use_pq can only be applied to vectors with greater than {} dimensions. {} dimensions provided", opt.pq_vector_length, dimensions)
        };
        if dimensions % opt.pq_vector_length as i32 != 0 {
            error!("use_pq can only be applied to vectors where the number of dimensions {} is divisible by the pq_vector_length {} ", dimensions, opt.pq_vector_length)
        };
    }
    assert!(dimensions > 0 && dimensions < 2000);
    let meta_page = unsafe { MetaPage::create(&index_relation, dimensions as _, opt.clone()) };
    let ntuples = do_heap_scan(index_info, &heap_relation, &index_relation, meta_page);

    let mut result = unsafe { PgBox::<pg_sys::IndexBuildResult>::alloc0() };
    result.heap_tuples = ntuples as f64;
    result.index_tuples = ntuples as f64;

    result.into_pg()
}

#[pg_guard]
pub unsafe extern "C" fn aminsert(
    indexrel: pg_sys::Relation,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    heap_tid: pg_sys::ItemPointer,
    heaprel: pg_sys::Relation,
    _check_unique: pg_sys::IndexUniqueCheck,
    _index_unchanged: bool,
    index_info: *mut pg_sys::IndexInfo,
) -> bool {
    let index_relation = unsafe { PgRelation::from_pg(indexrel) };
    let heap_relation = unsafe { PgRelation::from_pg(heaprel) };
    let vec = PgVector::from_pg_parts(values, isnull, 0);
    if let None = vec {
        //todo handle NULLs?
        return false;
    }
    let vec = vec.unwrap();
    let vector = (*vec).to_slice();
    let heap_pointer = ItemPointer::with_item_pointer_data(*heap_tid);
    let meta_page = MetaPage::read(&index_relation);

    let mut quantizer = meta_page.get_quantizer();
    match &mut quantizer {
        Quantizer::None => {}
        Quantizer::PQ(pq) => {
            pq.load(&index_relation, &meta_page);
        }
        Quantizer::BQ(bq) => {
            bq.load(&index_relation, &meta_page);
        }
    }

    let vp = VectorProvider::new(
        Some(&heap_relation),
        Some(get_attribute_number(index_info)),
        &quantizer,
        false,
    );
    let mut graph = DiskIndexGraph::new(&index_relation, vp);

    let node = model::Node::new(vector.to_vec(), heap_pointer, &meta_page, &quantizer);

    let mut tape = unsafe { Tape::new(&index_relation, page::PageType::Node) };
    let index_pointer: IndexPointer = node.write(&mut tape);

    let _stats = graph.insert(&index_relation, index_pointer, vector);
    false
}

#[pg_guard]
pub extern "C" fn ambuildempty(_index_relation: pg_sys::Relation) {
    panic!("ambuildempty: not yet implemented")
}

fn get_attribute_number(index_info: *mut pg_sys::IndexInfo) -> pg_sys::AttrNumber {
    unsafe { assert!((*index_info).ii_NumIndexAttrs == 1) };
    unsafe { (*index_info).ii_IndexAttrNumbers[0] }
}

fn do_heap_scan<'a>(
    index_info: *mut pg_sys::IndexInfo,
    heap_relation: &'a PgRelation,
    index_relation: &'a PgRelation,
    meta_page: MetaPage,
) -> usize {
    let quantizer = meta_page.get_quantizer();
    let vp = VectorProvider::new(
        Some(heap_relation),
        Some(get_attribute_number(index_info)),
        &quantizer,
        false,
    );

    let bg = BuilderGraph::new(meta_page.clone(), vp);
    let quantizer = meta_page.get_quantizer();
    let mut state = BuildState::new(index_relation, meta_page.clone(), bg, quantizer);
    unsafe {
        pg_sys::IndexBuildHeapScan(
            heap_relation.as_ptr(),
            index_relation.as_ptr(),
            index_info,
            Some(build_callback),
            &mut state,
        );
    }

    // we train the quantizer and add prepare to write quantized values to the nodes.
    match &mut state.quantizer {
        Quantizer::None => {}
        Quantizer::PQ(pq) => {
            pq.finish_training();
        }
        Quantizer::BQ(bq) => {
            bq.finish_training();
        }
    }

    let write_stats = unsafe { state.node_builder.write(index_relation, &state.quantizer) };
    assert_eq!(write_stats.num_nodes, state.ntuples);

    let writing_took = Instant::now()
        .duration_since(write_stats.started)
        .as_secs_f64();
    if write_stats.num_nodes > 0 {
        info!(
            "Writing took {}s or {}s/tuple.  Avg neighbors: {}",
            writing_took,
            writing_took / write_stats.num_nodes as f64,
            write_stats.num_neighbors / write_stats.num_nodes
        );
    }
    if write_stats.num_prunes > 0 {
        info!(
            "When pruned for cleanup: avg neighbors before/after {}/{} of {} prunes",
            write_stats.num_neighbors_before_prune / write_stats.num_prunes,
            write_stats.num_neighbors_after_prune / write_stats.num_prunes,
            write_stats.num_prunes
        );
    }
    let ntuples = state.ntuples;

    warning!("Indexed {} tuples", ntuples);

    match state.quantizer {
        Quantizer::None => {}
        Quantizer::PQ(pq) => {
            pq.write_metadata(index_relation);
        }
        Quantizer::BQ(bq) => {
            bq.write_metadata(index_relation);
        }
    }

    ntuples
}

#[pg_guard]
unsafe extern "C" fn build_callback(
    index: pg_sys::Relation,
    ctid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    _tuple_is_alive: bool,
    state: *mut std::os::raw::c_void,
) {
    let index_relation = unsafe { PgRelation::from_pg(index) };
    let vec = PgVector::from_pg_parts(values, isnull, 0);
    if let Some(vec) = vec {
        let state = (state as *mut BuildState).as_mut().unwrap();

        let mut old_context = state.memcxt.set_as_current();
        let heap_pointer = ItemPointer::with_item_pointer_data(*ctid);

        build_callback_internal(index_relation, heap_pointer, (*vec).to_slice(), state);

        old_context.set_as_current();
        state.memcxt.reset();
    }
    //todo: what do we do with nulls?
}

#[inline(always)]
fn build_callback_internal(
    index: PgRelation,
    heap_pointer: ItemPointer,
    vector: &[f32],
    state: &mut BuildState,
) {
    check_for_interrupts!();

    state.ntuples = state.ntuples + 1;

    if state.ntuples % 1000 == 0 {
        info!(
            "Processed {} tuples in {}s which is {}s/tuple. Dist/tuple: Prune: {} search: {}. Stats: {:?}",
            state.ntuples,
            Instant::now().duration_since(state.started).as_secs_f64(),
            (Instant::now().duration_since(state.started) / state.ntuples as u32).as_secs_f64(),
            state.stats.prune_neighbor_stats.distance_comparisons / state.ntuples,
            state.stats.greedy_search_stats.distance_comparisons / state.ntuples,
            state.stats,
        );
    }

    match &mut state.quantizer {
        Quantizer::None => {}
        Quantizer::PQ(pq) => {
            pq.add_sample(vector);
        }
        Quantizer::BQ(bq) => {
            bq.add_sample(vector);
        }
    }

    let node = model::Node::new(
        vector.to_vec(),
        heap_pointer,
        &state.meta_page,
        &state.quantizer,
    );

    let index_pointer: IndexPointer = node.write(&mut state.tape);
    let new_stats = state.node_builder.insert(&index, index_pointer, vector);
    state.stats.combine(new_stats);
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::*;

    #[pg_test]
    unsafe fn test_index_creation() -> spi::Result<()> {
        Spi::run(&format!(
            "CREATE TABLE test(embedding vector(3));

            INSERT INTO test(embedding) VALUES ('[1,2,3]'), ('[4,5,6]'), ('[7,8,10]');

            CREATE INDEX idxtest
                  ON test
               USING tsv(embedding)
                WITH (num_neighbors=30);

            set enable_seqscan =0;
            select * from test order by embedding <=> '[0,0,0]';
            explain analyze select * from test order by embedding <=> '[0,0,0]';
            drop index idxtest;
            ",
        ))?;
        Ok(())
    }

    #[pg_test]
    unsafe fn test_pq_index_creation() -> spi::Result<()> {
        Spi::run(&format!(
            "CREATE TABLE test_pq (
                embedding vector (1536)
            );

           -- generate 300 vectors
            INSERT INTO test_pq (embedding)
            SELECT
                *
            FROM (
                SELECT
                    ('[' || array_to_string(array_agg(random()), ',', '0') || ']')::vector AS embedding
                FROM
                    generate_series(1, 1536 * 300) i
                GROUP BY
                    i % 300) g;

            CREATE INDEX idx_tsv_pq ON test_pq USING tsv (embedding) WITH (num_neighbors = 64, search_list_size = 125, max_alpha = 1.0, use_pq = TRUE, pq_vector_length = 64);

            ;

            SET enable_seqscan = 0;
            -- perform index scans on the vectors
            SELECT
                *
            FROM
                test_pq
            ORDER BY
                embedding <=> (
                    SELECT
                        ('[' || array_to_string(array_agg(random()), ',', '0') || ']')::vector AS embedding
            FROM generate_series(1, 1536));

            EXPLAIN ANALYZE
            SELECT
                *
            FROM
                test_pq
            ORDER BY
                embedding <=> (
                    SELECT
                        ('[' || array_to_string(array_agg(random()), ',', '0') || ']')::vector AS embedding
            FROM generate_series(1, 1536));

            DROP INDEX idx_tsv_pq;
            ",
        ))?;
        Ok(())
    }

    #[pg_test]
    unsafe fn test_bq_index_creation() -> spi::Result<()> {
        Spi::run(&format!(
            "CREATE TABLE test_bq (
                embedding vector (1536)
            );

           -- generate 300 vectors
            INSERT INTO test_bq (embedding)
            SELECT
                *
            FROM (
                SELECT
                    ('[' || array_to_string(array_agg(random()), ',', '0') || ']')::vector AS embedding
                FROM
                    generate_series(1, 1536 * 300) i
                GROUP BY
                    i % 300) g;

            CREATE INDEX idx_tsv_pq ON test_bq USING tsv (embedding) WITH (num_neighbors = 64, search_list_size = 125, max_alpha = 1.0, use_bq = TRUE);

            ;

            SET enable_seqscan = 0;
            -- perform index scans on the vectors
            SELECT
                *
            FROM
                test_bq
            ORDER BY
                embedding <=> (
                    SELECT
                        ('[' || array_to_string(array_agg(random()), ',', '0') || ']')::vector AS embedding
            FROM generate_series(1, 1536));

            EXPLAIN ANALYZE
            SELECT
                *
            FROM
                test_bq
            ORDER BY
                embedding <=> (
                    SELECT
                        ('[' || array_to_string(array_agg(random()), ',', '0') || ']')::vector AS embedding
            FROM generate_series(1, 1536));

            DROP INDEX idx_tsv_pq;
            ",
        ))?;
        Ok(())
    }

    #[pg_test]
    unsafe fn test_insert() -> spi::Result<()> {
        Spi::run(&format!(
            "CREATE TABLE test(embedding vector(3));

            INSERT INTO test(embedding) VALUES ('[1,2,3]'), ('[4,5,6]'), ('[7,8,10]');

            CREATE INDEX idxtest
                  ON test
               USING tsv(embedding)
                WITH (num_neighbors=30);

            INSERT INTO test(embedding) VALUES ('[11,12,13]');
            ",
        ))?;

        let res: Option<i64> = Spi::get_one(&format!(
            "   set enable_seqscan = 0;
                WITH cte as (select * from test order by embedding <=> '[0,0,0]') SELECT count(*) from cte;",
        ))?;
        assert_eq!(4, res.unwrap());

        Spi::run(&format!(
            "INSERT INTO test(embedding) VALUES ('[11,12,13]'),  ('[14,15,16]');",
        ))?;
        let res: Option<i64> = Spi::get_one(&format!(
            "   set enable_seqscan = 0;
                WITH cte as (select * from test order by embedding <=> '[0,0,0]') SELECT count(*) from cte;",
        ))?;
        assert_eq!(6, res.unwrap());

        Spi::run(&format!("drop index idxtest;",))?;

        Ok(())
    }

    #[pg_test]
    unsafe fn test_empty_table_insert() -> spi::Result<()> {
        Spi::run(&format!(
            "CREATE TABLE test(embedding vector(3));

            CREATE INDEX idxtest
                  ON test
               USING tsv(embedding)
                WITH (num_neighbors=30);

            INSERT INTO test(embedding) VALUES ('[1,2,3]'), ('[4,5,6]'), ('[7,8,10]');
            ",
        ))?;

        let res: Option<i64> = Spi::get_one(&format!(
            "   set enable_seqscan = 0;
                WITH cte as (select * from test order by embedding <=> '[0,0,0]') SELECT count(*) from cte;",
        ))?;
        assert_eq!(3, res.unwrap());

        Spi::run(&format!("drop index idxtest;",))?;

        Ok(())
    }

    #[pg_test]
    unsafe fn test_insert_empty_insert() -> spi::Result<()> {
        Spi::run(&format!(
            "CREATE TABLE test(embedding vector(3));

            CREATE INDEX idxtest
                  ON test
               USING tsv(embedding)
                WITH (num_neighbors=30);

            INSERT INTO test(embedding) VALUES ('[1,2,3]'), ('[4,5,6]'), ('[7,8,10]');
            DELETE FROM test;
            INSERT INTO test(embedding) VALUES ('[1,2,3]'), ('[14,15,16]');
            ",
        ))?;

        let res: Option<i64> = Spi::get_one(&format!(
            "   set enable_seqscan = 0;
                WITH cte as (select * from test order by embedding <=> '[0,0,0]') SELECT count(*) from cte;",
        ))?;
        assert_eq!(2, res.unwrap());

        Spi::run(&format!("drop index idxtest;",))?;

        Ok(())
    }
}
