# Wizard shows template packs at the template version

## Problem

After `claude@2` was added, a template with `claude@1` showed nothing
special in the setup wizard. The wizard always used the newest pack
version for every entry, and parsed the template arg values against the
newest args. If a newer version adds or removes args, the wizard showed
args that the template sandbox does not have, and dropped the old ones.

## Change

- Each wizard entry keeps the newest pack and the shown pack. When the
  user selects a template, each template pack swaps to the version that
  the template config pins (looked up from the loaded packs). The arg
  rows and the template arg values use that version.
- The empty template, or a switch to another template, sets the shown
  pack back to the newest version together with the saved user state.
- A template pack whose shown version differs from the newest gets a red
  `! outdated` marker after its label.
- If the pinned version is not known (for example, a template from a
  newer airlock), the pack row shows the newest version, and the
  template row gets the `! missing` marker (see below).
- The answers never contain template packs, so the swapped version does
  not reach the generated config. The template config keeps its version.

The lookup is a sync helper on the pack set. Config resolve uses it
through the async resolve, which stays async for later remote packs.

## Changed and missing template packs

User packs have no version, so "outdated" does not apply to them. Two
more markers cover them:

- `! changed` on a template pack row: the template install snapshot has
  the file hashes of each installed disk pack (by install fingerprint in
  the install records). The wizard reads them once per template and
  compares them with the file hashes of the loaded pack. A difference
  means that the start asks the changed-packs question. Built-in packs
  have no file copies in the snapshot, and packs without a setup script
  have no install record, so they never show as changed. A template of
  an older airlock has no snapshots, so nothing shows.
- `! missing: <ids>` on a template row: a `[packs]` entry of the
  template config, or its pinned version, is not in the loaded packs.
  The pack has no wizard row, so the marker goes after the template
  name.

The file comparison covers only file changes. Arg changes cannot happen
(the template config keeps the args), and setup value changes are only
known after the pack config runs at start.
