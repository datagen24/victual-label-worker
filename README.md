# victual-label-worker

The delivery worker [ADR-0019](https://github.com/datagen24/victual/blob/master/docs/adr/0019-label-printers-are-master-data.md)
decides and [plan 25](https://github.com/datagen24/victual/blob/master/docs/plans/25-label-infrastructure.md)
piece 4 specifies. It claims print jobs over Victual's authenticated pull API, verifies the
artifact it was handed against the printer's resolved configuration, encodes it for the device
and reports what happened.

**It renders nothing.** Under [ADR-0021](https://github.com/datagen24/victual/blob/master/docs/adr/0021-label-templates-are-application-data.md)
the template document is Victual's and the rasterizing belongs to
[victual-label-renderer](https://github.com/datagen24/victual-label-renderer). What arrives
here is a validated artifact and a manifest, and what this does is check the two against the
device it is holding.

**It holds no database credential and makes no database connection.** It authenticates with a
typed API key over nine routes. A remote worker would otherwise need database reachability
from wherever it sits, which is a worse thing to expose across a network boundary than an
authenticated HTTP endpoint with a revocable key — and it would couple an independently
released repository to Victual's migrations.

## The rules it exists to keep

- **The combination is revalidated immediately before device I/O.** Victual already refused
  impossible jobs at enqueue; media and driver versions can change in between. A job whose
  `(model, media, resolution, colour mode)` is not in the driver's `combinations` results in
  **zero bytes reaching the device**, counted rather than assumed. That is not hypothetical:
  on 2026-09-07 a two-colour job at 600 dpi was assembled and sent, and a QL-820NWBc refused
  it with "Communications Command Error" although the capability document already said that
  combination does not exist.
- **The artifact's grid is the profile's.** `raster/png-indexed;v=1` declares
  `geometry: fixed_grid`, so a raster whose width is not the device's printable dot count is a
  refusal rather than a resize. Issue [#90](https://github.com/datagen24/victual/issues/90) is
  what a resize costs: the prototype authored against `dots_total` while its library compared
  against `dots_printable`, so every endless print was silently resampled — twice, at 600 dpi,
  turning a 900-pixel label into 1,711 pixels.
- **No automatic redispatch after a claimed attempt.** A failure is reported and ends there. A
  person authorizes another attempt, naming the one they reviewed. A worker killed between
  writing bytes and posting its result leaves an attempt that is *uncertain*, which is the
  honest state: redispatching would guess, and it would guess in the direction that prints.
- **An unreported outcome is not discarded because a credential was refused.** A 401 on
  `result` means retry after rotating; it does not mean the outcome is lost.

## Transports

`tcp` — raw port 9100. Constants come from `brother_ql-inventree` 1.3 rather than being
invented: 90 bytes per row and 400 invalidate bytes from its `models.py`, and 732/696/12/35 for
label `62` from its `labels.py`. Delivery evidence over this path is `transport`: the device
answers no status request, so *sent* is all it can honestly report.

`ipp` — Internet Printing Protocol. The same byte stream as `application/octet-stream`, which
the QL-820NWBc treats as a passthrough, plus `job-state` and `job-impressions-completed`
afterwards — so this path can report `device_reported` where raw 9100 cannot. What IPP
*declares* about the device understates it: it advertises `monochrome` and 300 dpi, and the
identical two-colour raster printed anyway on 2026-09-07 and reached
`job-state = completed`.

## Physically demonstrated, 2026-09-09

A label requested in Victual, rendered, verified, claimed here and printed on a QL-820NWBc at
`10.130.30.94` over IPP: the device answered `job-state = 9` (completed) with
`job-impressions-completed = 1`, which is `device_reported` evidence rather than "the bytes
left here". It scanned back to the location it names, and it measured right way round at the
size the profile fixes — which settles issue
[#90](https://github.com/datagen24/victual/issues/90)'s rotation sign by a printed label
rather than by reading.

**Two-colour printed on 2026-09-09 too**, through the whole path rather than from a standalone
encoder: a filled red band with white text knocked out of it, under a black location name and
QR — 27,306 red pixels in the artifact, and red on the tape. Red on a QL is a property of the
**roll** rather than of an ink well: DK-22251 carries a layer that develops red at a different
temperature, so "two-colour" is a second raster plane the device burns at a different energy.
That is why the roll and not the artifact's ink decides whether the two-plane stream is sent.

**The version rule caught a change to this document.** Correcting the provenance above altered
a capability document that had already been registered, and Victual refused the
re-registration: `A registered driver version is immutable`. The document is `1.1` now, which
is what ADR-0019 decision item 3 asks for — a worker whose capabilities changed publishes a new
version. The printer was then moved to it by an explicit revalidating admin action, and the job
that was queued against `1.0` in between recorded a visible `blocked` attempt naming what was
missing, with provably zero bytes sent.

Two defects that run found, both in this repository and both fixed before anything printed:

- **Two-colour was read from the artifact's ink rather than from the loaded roll.** A
  black-only label on 62red tape was being sent as a single-plane stream, which is a media
  mismatch — and a media mismatch is what "Wrong Roll Type" means. The resolved combination
  decides now, so a black-only label on red tape sends an empty red plane.
- **`--dry-run` consumed an authorization and reported nothing**, leaving a job that looked
  attempted while no bytes had been sent. It records the attempt as failed saying so, and a
  person authorizes the real one.

The first attempt of the day declared plain `62` while 62red was loaded. The device latched
into an error state and the next two jobs could not have succeeded whatever they carried,
which is worth knowing: over raw 9100 the worker reported *sent* for all three, because
*sent* is the most that path can honestly report. The IPP path is what turned a guess into a
report.

## Running it

```
victual-label-worker --api https://victual.example --key-file /run/secrets/worker-key
```

`--health` exits 0 without contacting anything. `--once` claims at most one job and exits.
