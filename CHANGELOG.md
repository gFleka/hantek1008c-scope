# Changelog

All notable changes to LR Scope 1008C. Versions follow [SemVer](https://semver.org); tags are `vX.Y.Z`.

## [0.1.1] — 2026-11-23

### Added
- **Measurement table on / off**: **F3**, or click its title (▾ / ▸ Measurements). Hidden, it shrinks to one line
  and the waveform gets the room (with 8 channels about 40 % more graph height); statistics keep adding up, the
  choice is saved.

### Fixed
- **Guided test "Ignition primary (coil −) voltage"**: choosing ignition primary from the guided tests now sets up
  the channel like choosing it as the channel's signal type: ×20 attenuator, 100 V/div (firing spike on screen),
  1 ms/div, trigger at the dwell start and the dwell / spike / burn / ringing analysis with a verdict. Before, the
  only ignition-primary entry was the current-clamp test, which switched no analysis on.

## [0.1.0] — 2026-11-23

First release (used locally, not published). 0.1.1 is the first release on GitHub.
