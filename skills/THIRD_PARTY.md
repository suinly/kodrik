# Bundled third-party skills

These are selected skills and supporting files from upstream snapshots, adapted for Codrik. The archived manifests were copied from local installations; their hashes identify the upstream archives, not this adapted bundle. No upstream plugins, installed hooks, startup injection, companion server, or automatic updater are included.

| Project | Snapshot | Source | Upstream provenance | MIT notice |
| --- | --- | --- | --- | --- |
| Ponytail | 5.3.3 | [DietrichGebert/ponytail](https://github.com/DietrichGebert/ponytail) | [Manifest](KODRIK-INSTALL-ponytail.json) | [License](LICENSE-ponytail) |
| Superpowers | 7.0.1 | [obra/superpowers](https://github.com/obra/superpowers) | [Manifest](KODRIK-INSTALL-superpowers.json) | [License](LICENSE-superpowers) |
| Taste Skill | 2.5.1 | [Leonxlnx/taste-skill](https://github.com/Leonxlnx/taste-skill) | [Commit `952435f`](https://github.com/Leonxlnx/taste-skill/tree/952435f63acece7d2fcb4b4d3c1786c1c43603e0) | [License](LICENSE-taste-skill) |

The snapshots contain six Ponytail and fifteen Superpowers skill directories, including `diagnosing-superpowers`. The bundle keeps the Ponytail benchmark report and selected Superpowers prompts, templates, references, examples, and on-demand helpers. It omits upstream plugin metadata, host-specific tool mappings, the custom visual companion, and unreferenced test and creation history. [Codrik adaptation](KODRIK.md) defines the host-specific rules.

Taste Skill is bundled as `design-taste-frontend`, matching its upstream skill name. The bundle includes its assets, library, references, and on-demand scripts, with upstream credits kept alongside the assets. The Claude Code stop hook was removed from the skill metadata; the finish gate runs only when invoked manually. Isolated Bashkit spaces do not extract the kit.
