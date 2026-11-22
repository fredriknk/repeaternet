"""Censored mixed model for measured link signal versus predicted path loss.

Model for link i from node t to node r (all quantities in dB):

    y_i = c[b_i] + u[t_i] + w[r_i] + X_i . beta - PL_i + e_i,   e_i ~ N(0, sigma^2)

* ``y`` is a link-level signal (dBm, or SNR in dB for SNR-only evidence).
* ``c[b]`` is a budget constant per measurement block (TX power + gains -
  feed losses, or minus the noise floor for SNR). It has an optional normal
  prior, so the absolute bias stays honest about the unknown hardware.
* ``u`` and ``w`` are transmit- and receive-side node offsets with normal
  priors N(0, sigma_u^2) and N(0, sigma_w^2). They absorb unknown power,
  gain, feed loss, noise floor and siting.
* ``X . beta`` holds optional correction covariates (fitted on training data
  only).
* Links can be censored: ``cens = -1`` means y < bound (never heard),
  ``cens = +1`` means y > bound (saturated SNR).

Fitting is EM for a Tobit model: censored links are replaced by their
conditional means under the current fit, and the M-step solves a ridge
system whose penalties are the variance ratios. The variance components are
updated from the residuals and the posterior variances.

Only numpy and scipy are used.
"""

from __future__ import annotations

from dataclasses import dataclass, field

import numpy as np
import scipy.sparse as sp
from scipy.sparse.linalg import spsolve
from scipy.special import log_ndtr
from scipy.stats import norm

MIN_SIGMA = 0.3


@dataclass
class LinkData:
    """Link-level arrays for one engine configuration."""

    y: np.ndarray  # (n,)
    cens: np.ndarray  # (n,) int in {-1, 0, +1}
    bound: np.ndarray  # (n,) censoring bound (ignored when cens == 0)
    block: np.ndarray  # (n,) int block index
    tx: np.ndarray  # (n,) int node index
    rx: np.ndarray  # (n,) int node index
    pl: np.ndarray  # (n,) predicted path loss, dB
    x: np.ndarray | None = None  # (n, p) correction covariates
    n_blocks: int = 1
    n_nodes: int = 0

    def subset(self, mask: np.ndarray, pl: np.ndarray | None = None) -> "LinkData":
        return LinkData(
            y=self.y[mask],
            cens=self.cens[mask],
            bound=self.bound[mask],
            block=self.block[mask],
            tx=self.tx[mask],
            rx=self.rx[mask],
            pl=(self.pl if pl is None else pl)[mask],
            x=None if self.x is None else self.x[mask],
            n_blocks=self.n_blocks,
            n_nodes=self.n_nodes,
        )


@dataclass
class Priors:
    """Priors on block constants and variance components."""

    c_mean: np.ndarray  # (n_blocks,)
    c_sd: np.ndarray  # (n_blocks,) use np.inf for a flat prior
    node_effects: bool = True
    sigma_u0: float = 3.0
    sigma_w0: float = 3.0
    fix_node_sd: bool = False


@dataclass
class Fit:
    c: np.ndarray
    u: np.ndarray  # (n_nodes,) tx offsets, 0 for nodes without tx links
    w: np.ndarray  # (n_nodes,) rx offsets
    beta: np.ndarray
    sigma: float
    sigma_u: float
    sigma_w: float
    mu: np.ndarray  # fitted link means
    loglik: float
    iterations: int
    converged: bool
    has_tx: np.ndarray = field(default_factory=lambda: np.zeros(0, bool))
    has_rx: np.ndarray = field(default_factory=lambda: np.zeros(0, bool))

    def predict(self, d: LinkData, with_nodes: bool = True) -> np.ndarray:
        mu = self.c[d.block] - d.pl
        if d.x is not None and self.beta.size:
            mu = mu + d.x @ self.beta
        if with_nodes:
            mu = mu + self.u[d.tx] + self.w[d.rx]
        return mu


def _truncated_moments(y, mu, s, bound, cens):
    """E-step: observed values, or the conditional mean and variance of
    N(mu, s^2) given the censoring."""
    a = (bound - mu) / s
    mean = np.where(cens == 0, y, mu)
    var = np.zeros_like(mu)
    lo = cens == -1  # y < bound
    if lo.any():
        aa = a[lo]
        lam = np.exp(norm.logpdf(aa) - log_ndtr(aa))
        mean[lo] = mu[lo] - s * lam
        var[lo] = s * s * np.clip(1.0 - aa * lam - lam * lam, 1e-6, 1.0)
    hi = cens == 1  # y > bound
    if hi.any():
        aa = a[hi]
        lam = np.exp(norm.logpdf(aa) - log_ndtr(-aa))
        mean[hi] = mu[hi] + s * lam
        var[hi] = s * s * np.clip(1.0 + aa * lam - lam * lam, 1e-6, 1.0)
    return mean, var


def loglik_terms(d: LinkData, mu: np.ndarray, sigma: float) -> np.ndarray:
    """Per-link log likelihood under N(mu, sigma^2) with censoring."""
    out = np.empty_like(mu)
    sigma = np.broadcast_to(np.asarray(sigma, float), mu.shape)
    obs = d.cens == 0
    out[obs] = norm.logpdf(d.y[obs], mu[obs], sigma[obs])
    lo = d.cens == -1
    out[lo] = log_ndtr((d.bound[lo] - mu[lo]) / sigma[lo])
    hi = d.cens == 1
    out[hi] = log_ndtr((mu[hi] - d.bound[hi]) / sigma[hi])
    return out


def fit(d: LinkData, pri: Priors, max_iter: int = 200, tol: float = 1e-4) -> Fit:
    """EM fit of the censored mixed model."""
    n = d.y.size
    nb, nn = d.n_blocks, d.n_nodes
    p = 0 if d.x is None else d.x.shape[1]
    tx_nodes = np.unique(d.tx)
    rx_nodes = np.unique(d.rx)
    use_nodes = pri.node_effects
    ntx = tx_nodes.size if use_nodes else 0
    nrx = rx_nodes.size if use_nodes else 0
    m = nb + ntx + nrx + p
    rows = [np.arange(n)]
    cols = [d.block]
    vals = [np.ones(n)]
    if use_nodes:
        rows += [np.arange(n), np.arange(n)]
        cols += [nb + np.searchsorted(tx_nodes, d.tx), nb + ntx + np.searchsorted(rx_nodes, d.rx)]
        vals += [np.ones(n), np.ones(n)]
    if p:
        rr, cc = np.meshgrid(np.arange(n), np.arange(p), indexing="ij")
        rows.append(rr.ravel())
        cols.append(nb + ntx + nrx + cc.ravel())
        vals.append(d.x.ravel())
    a = sp.csr_matrix((np.concatenate(vals), (np.concatenate(rows), np.concatenate(cols))), shape=(n, m))
    ata = (a.T @ a).tocsc()
    # Links per tx and rx node, for the diagonal posterior-variance approximation.
    n_tx = np.asarray(ata.diagonal()[nb : nb + ntx]) if use_nodes else np.zeros(0)
    n_rx = np.asarray(ata.diagonal()[nb + ntx : nb + ntx + nrx]) if use_nodes else np.zeros(0)

    sigma = 6.0
    su, sw = pri.sigma_u0, pri.sigma_w0
    theta = np.zeros(m)
    theta[:nb] = np.where(np.isfinite(pri.c_sd), pri.c_mean, 0.0)
    # Initialise flat-prior blocks at their mean offset.
    for b in range(nb):
        sel = (d.block == b) & (d.cens == 0)
        if not np.isfinite(pri.c_sd[b]) and sel.any():
            theta[b] = np.mean(d.y[sel] + d.pl[sel])
    mu = a @ theta - d.pl
    prev = -np.inf
    converged = False
    it = 0
    for it in range(1, max_iter + 1):
        ystar, vstar = _truncated_moments(d.y, mu, sigma, d.bound, d.cens)
        z = ystar + d.pl
        pen = np.zeros(m)
        prior_mean = np.zeros(m)
        s2 = sigma * sigma
        for b in range(nb):
            if np.isfinite(pri.c_sd[b]):
                pen[b] = s2 / pri.c_sd[b] ** 2
                prior_mean[b] = pri.c_mean[b]
            else:
                pen[b] = 1e-8
        if use_nodes:
            pen[nb : nb + ntx] = s2 / max(su, MIN_SIGMA) ** 2
            pen[nb + ntx : nb + ntx + nrx] = s2 / max(sw, MIN_SIGMA) ** 2
        if p:
            pen[nb + ntx + nrx :] = 1e-6
        lhs = (ata + sp.diags(pen)).tocsc()
        theta = spsolve(lhs, a.T @ z + pen * prior_mean)
        mu = a @ theta - d.pl
        resid = ystar - mu
        sigma = float(np.sqrt(max((resid @ resid + vstar.sum()) / n, MIN_SIGMA**2)))
        if use_nodes and not pri.fix_node_sd:
            # Posterior variances of the node effects, diagonal approximation
            # sigma^2 / (links + sigma^2 / sigma_node^2).
            ut = theta[nb : nb + ntx]
            wt = theta[nb + ntx : nb + ntx + nrx]
            s2 = sigma * sigma
            if ntx:
                var_u = s2 / (n_tx + s2 / max(su, MIN_SIGMA) ** 2)
                su = float(np.sqrt(max((ut @ ut + var_u.sum()) / ntx, MIN_SIGMA**2)))
            if nrx:
                var_w = s2 / (n_rx + s2 / max(sw, MIN_SIGMA) ** 2)
                sw = float(np.sqrt(max((wt @ wt + var_w.sum()) / nrx, MIN_SIGMA**2)))
        ll = float(loglik_terms(d, mu, sigma).sum())
        if abs(ll - prev) < tol * max(1.0, abs(ll)):
            converged = True
            break
        prev = ll

    c = theta[:nb].copy()
    u = np.zeros(nn)
    w = np.zeros(nn)
    has_tx = np.zeros(nn, bool)
    has_rx = np.zeros(nn, bool)
    if use_nodes:
        u[tx_nodes] = theta[nb : nb + ntx]
        w[rx_nodes] = theta[nb + ntx : nb + ntx + nrx]
        has_tx[tx_nodes] = True
        has_rx[rx_nodes] = True
    beta = theta[nb + ntx + nrx :].copy()
    return Fit(
        c=c,
        u=u,
        w=w,
        beta=beta,
        sigma=sigma,
        sigma_u=su if use_nodes else 0.0,
        sigma_w=sw if use_nodes else 0.0,
        mu=mu,
        loglik=float(loglik_terms(d, mu, sigma).sum()),
        iterations=it,
        converged=converged,
        has_tx=has_tx,
        has_rx=has_rx,
    )


# ---------------------------------------------------------------------------
# Antenna heights
# ---------------------------------------------------------------------------

GRID_HEIGHTS = np.array([2.0, 5.0, 10.0, 20.0])


def interp_pl(grid: np.ndarray, h_tx: np.ndarray, h_rx: np.ndarray, heights=GRID_HEIGHTS) -> np.ndarray:
    """Bilinear interpolation of PL grids (n, H, H) in log height.

    ``grid[i, a, b]`` is the loss for tx height ``heights[a]`` and rx height
    ``heights[b]``. Heights outside the grid are clamped. NaN cells propagate.
    """
    lh = np.log(heights)

    def locate(h):
        x = np.clip(np.log(np.asarray(h, float)), lh[0], lh[-1])
        j = np.clip(np.searchsorted(lh, x, side="right") - 1, 0, lh.size - 2)
        t = (x - lh[j]) / (lh[j + 1] - lh[j])
        return j, t

    n = grid.shape[0]
    ja, ta = locate(np.broadcast_to(h_tx, (n,)))
    jb, tb = locate(np.broadcast_to(h_rx, (n,)))
    i = np.arange(n)
    g00 = grid[i, ja, jb]
    g01 = grid[i, ja, jb + 1]
    g10 = grid[i, ja + 1, jb]
    g11 = grid[i, ja + 1, jb + 1]
    return (1 - ta) * ((1 - tb) * g00 + tb * g01) + ta * ((1 - tb) * g10 + tb * g11)


CANDIDATE_HEIGHTS = np.array([2.0, 3.0, 5.0, 7.0, 10.0, 14.0, 20.0])


def fit_with_heights(
    d: LinkData,
    grid: np.ndarray,
    h_prior: np.ndarray,
    pri: Priors,
    prior_sd_log: float = np.log(2.0),
    rounds: int = 3,
) -> tuple[Fit, np.ndarray]:
    """Alternate the model fit with a per-node antenna height choice.

    ``h_prior`` (n_nodes,) is each node's prior height. Each node picks the
    candidate height that maximises its links' likelihood plus a log-normal
    prior around ``h_prior``. Returns the fit and the chosen heights.
    """
    h = h_prior.astype(float).copy()
    cur = d
    f = None
    nodes = np.unique(np.concatenate([d.tx, d.rx]))
    for _ in range(rounds):
        cur = d.subset(np.ones(d.y.size, bool), pl=interp_pl(grid, h[d.tx], h[d.rx]))
        f = fit(cur, pri)
        changed = 0
        for j in nodes:
            sel = (d.tx == j) | (d.rx == j)
            sub = d.subset(sel)
            gsel = grid[sel]
            best, best_score = h[j], -np.inf
            is_tx = sub.tx == j
            is_rx = sub.rx == j
            s2 = f.sigma**2
            for cand in CANDIDATE_HEIGHTS:
                ht = np.where(is_tx, cand, h[sub.tx])
                hr = np.where(is_rx, cand, h[sub.rx])
                sub.pl = interp_pl(gsel, ht, hr)
                # Profile out node j's own offsets: shrunk mean residual of its
                # observed links on each side, then score with those offsets.
                base = f.predict(sub) - np.where(is_tx, f.u[j], 0.0) - np.where(is_rx, f.w[j], 0.0)
                obs = sub.cens == 0
                r = np.where(obs, sub.y - base, 0.0)
                uj = r[is_tx].sum() / (np.sum(obs & is_tx) + s2 / max(f.sigma_u, MIN_SIGMA) ** 2)
                wj = r[is_rx].sum() / (np.sum(obs & is_rx) + s2 / max(f.sigma_w, MIN_SIGMA) ** 2)
                if not pri.node_effects:
                    uj = wj = 0.0
                mu = base + np.where(is_tx, uj, 0.0) + np.where(is_rx, wj, 0.0)
                score = loglik_terms(sub, mu, f.sigma).sum()
                if pri.node_effects:
                    score += norm.logpdf(uj, 0, max(f.sigma_u, MIN_SIGMA))
                    score += norm.logpdf(wj, 0, max(f.sigma_w, MIN_SIGMA))
                score += norm.logpdf(np.log(cand), np.log(h_prior[j]), prior_sd_log)
                if np.isfinite(score) and score > best_score:
                    best, best_score = cand, score
            if best != h[j]:
                changed += 1
                h[j] = best
        if changed == 0:
            break
    cur = d.subset(np.ones(d.y.size, bool), pl=interp_pl(grid, h[d.tx], h[d.rx]))
    f = fit(cur, pri)
    return f, h


# ---------------------------------------------------------------------------
# Metrics
# ---------------------------------------------------------------------------


def auc(score_pos: np.ndarray, score_neg: np.ndarray) -> float:
    """Probability that a random positive scores above a random negative."""
    if score_pos.size == 0 or score_neg.size == 0:
        return float("nan")
    allv = np.concatenate([score_pos, score_neg])
    ranks = np.empty(allv.size)
    order = np.argsort(allv, kind="mergesort")
    ranks[order] = np.arange(1, allv.size + 1)
    # Average ties.
    _, inv, counts = np.unique(allv, return_inverse=True, return_counts=True)
    sums = np.zeros(counts.size)
    np.add.at(sums, inv, ranks)
    ranks = sums[inv] / counts[inv]
    rp = ranks[: score_pos.size].sum()
    return float((rp - score_pos.size * (score_pos.size + 1) / 2) / (score_pos.size * score_neg.size))


def summarize(err: np.ndarray) -> dict:
    """Bias, spread and quantiles of an error vector (measured minus predicted)."""
    err = err[np.isfinite(err)]
    if err.size == 0:
        return {"n": 0}
    q = np.quantile(err, [0.05, 0.1, 0.5, 0.9, 0.95])
    return {
        "n": int(err.size),
        "bias": float(err.mean()),
        "rmse": float(np.sqrt(np.mean(err**2))),
        "sd": float(err.std(ddof=1)) if err.size > 1 else float("nan"),
        "mae": float(np.mean(np.abs(err))),
        "p05": float(q[0]),
        "p10": float(q[1]),
        "p50": float(q[2]),
        "p90": float(q[3]),
        "p95": float(q[4]),
    }


def detection_floor_snr(sf: float | None) -> float:
    """LoRa demodulation SNR floor in dB by spreading factor (Semtech datasheet)."""
    table = {5: -2.5, 6: -5.0, 7: -7.5, 8: -10.0, 9: -12.5, 10: -15.0, 11: -17.5, 12: -20.0}
    if sf is None:
        return -10.0
    return table.get(int(sf), -10.0)


def censoring_corrected_mean(median: float, sigma_t: float, p_detect: float) -> float:
    """Link mean signal given the median of detected packets.

    With per-packet signal N(mu, sigma_t^2) and detection only above a floor
    that passes a fraction p of packets, the median of the detected packets
    satisfies P(S > m) = p / 2, so mu = m - sigma_t * Phi^-1(1 - p / 2).
    """
    p = float(np.clip(p_detect, 1e-3, 1.0))
    return float(median - sigma_t * norm.ppf(1.0 - p / 2.0))


def non_detection_bound(floor: float, sigma_t: float, opportunities: float) -> float:
    """Upper bound on a link mean for ``opportunities`` missed packets.

    All m packets fail with probability Phi((F - mu)/sigma_t)^m. Setting this
    to 0.5 gives mu = F - sigma_t * Phi^-1(0.5^(1/m)).
    """
    m = max(float(opportunities), 1.0)
    return float(floor - sigma_t * norm.ppf(0.5 ** (1.0 / m)))


__all__ = [
    "LinkData",
    "Priors",
    "Fit",
    "fit",
    "fit_with_heights",
    "interp_pl",
    "loglik_terms",
    "auc",
    "summarize",
    "detection_floor_snr",
    "censoring_corrected_mean",
    "non_detection_bound",
    "GRID_HEIGHTS",
    "CANDIDATE_HEIGHTS",
]
