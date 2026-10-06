# Slint link hit-test patch

`i-slint-core` is the unmodified crates.io 1.18.1 source package except for
`textlayout/sharedparley.rs`: accumulate (`|=`) hits across a selection's
wrapped-line rectangles, instead of overwriting the result for every line.
Otherwise only the final line of a wrapped link can be clicked or hovered.
Cargo's downloaded-package metadata and nested lockfile are omitted.

The GUI regression fixture checks destinations on multiple wrapped lines and
clipping. Keep this patch until an upstream Slint release includes the fix,
then remove the Cargo override and this directory together. Slint licenses
and copyright notices are included unchanged in the source package.
