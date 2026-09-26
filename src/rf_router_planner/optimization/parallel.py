from __future__ import annotations

import logging
import os
from collections.abc import Callable, Sequence
from concurrent.futures import ProcessPoolExecutor, as_completed

from rf_router_planner.models.link import LinkResult
from rf_router_planner.models.settings import RFSettings
from rf_router_planner.models.site import Site
from rf_router_planner.rf.propagation import LinkEvaluator
from rf_router_planner.terrain.raster import RasterTerrain, TerrainSource

logger = logging.getLogger(__name__)

type Pair = tuple[Site, Site]
type ProgressCallback = Callable[[int, int], None]
type CancelCallback = Callable[[], bool]

_process_terrain: RasterTerrain | None = None
_process_evaluator: LinkEvaluator | None = None


def _initialize_process(
    dtm_paths: list[str], dom_paths: list[str], settings: RFSettings
) -> None:
    global _process_terrain, _process_evaluator
    _process_terrain = RasterTerrain(dtm_paths, dom_paths)
    _process_evaluator = LinkEvaluator(_process_terrain, settings)


def _evaluate_process_batch(
    tasks: list[tuple[int, Site, Site, float]],
) -> list[tuple[int, LinkResult | None]]:
    if _process_evaluator is None:
        raise RuntimeError("RF worker was not initialized")
    results: list[tuple[int, LinkResult | None]] = []
    for index, source, target, sample_step_m in tasks:
        try:
            link = _process_evaluator.evaluate(source, target, sample_step_m)
            link.profile = None
            results.append((index, link))
        except ValueError:
            results.append((index, None))
    return results


def _sequential(
    terrain: TerrainSource,
    settings: RFSettings,
    pairs: Sequence[Pair],
    sample_step_m: float,
    progress: ProgressCallback,
    cancelled: CancelCallback,
) -> list[LinkResult]:
    evaluator = LinkEvaluator(terrain, settings)
    results: list[LinkResult] = []
    total = len(pairs)
    for index, (source, target) in enumerate(pairs, 1):
        if cancelled():
            break
        try:
            link = evaluator.evaluate(source, target, sample_step_m)
            link.profile = None
            results.append(link)
        except ValueError:
            pass
        if index % 20 == 0 or index == total:
            progress(index, total)
    return results


def evaluate_link_pairs(
    terrain: TerrainSource,
    settings: RFSettings,
    pairs: Sequence[Pair],
    sample_step_m: float,
    *,
    workers: int = 0,
    progress: ProgressCallback | None = None,
    cancelled: CancelCallback | None = None,
) -> list[LinkResult]:
    """Evaluate independent RF profiles across processes when terrain is reopenable."""
    notify = progress or (lambda _done, _total: None)
    is_cancelled = cancelled or (lambda: False)
    if not isinstance(terrain, RasterTerrain) or len(pairs) < 200 or workers == 1:
        return _sequential(terrain, settings, pairs, sample_step_m, notify, is_cancelled)
    worker_count = workers if workers > 0 else max(1, (os.cpu_count() or 2) - 1)
    worker_count = min(worker_count, len(pairs))
    ordered: list[LinkResult | None] = [None] * len(pairs)
    executor: ProcessPoolExecutor | None = None
    try:
        executor = ProcessPoolExecutor(
            max_workers=worker_count,
            initializer=_initialize_process,
            initargs=(terrain.dtm_paths, terrain.dom_paths, settings),
        )
        chunk_size = max(8, min(32, len(pairs) // max(1, worker_count * 8)))
        tasks = [
            (index, source, target, sample_step_m)
            for index, (source, target) in enumerate(pairs)
        ]
        batches = [tasks[start : start + chunk_size] for start in range(0, len(tasks), chunk_size)]
        futures = {
            executor.submit(_evaluate_process_batch, batch): len(batch) for batch in batches
        }
        completed = 0
        for future in as_completed(futures):
            if is_cancelled():
                for pending in futures:
                    pending.cancel()
                executor.shutdown(wait=False, cancel_futures=True)
                return [link for link in ordered if link is not None]
            batch_results = future.result()
            for index, link in batch_results:
                ordered[index] = link
            completed += len(batch_results)
            if completed % 20 == 0 or completed == len(pairs):
                notify(completed, len(pairs))
        executor.shutdown()
        executor = None
        return [link for link in ordered if link is not None]
    except Exception:
        logger.exception("Parallel RF evaluation failed; continuing on one process")
        if executor is not None:
            executor.shutdown(wait=False, cancel_futures=True)
        return _sequential(terrain, settings, pairs, sample_step_m, notify, is_cancelled)
