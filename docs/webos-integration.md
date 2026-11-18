# VectorCraft on WebOS

The `webos/vectorcraft-document-host` branch starts at VectorCraft
`4830ea0e833f1bd0dd923e734a9dfa3cb0ea5403`. Run the existing Trunk web app
inside a Desktop window with `?host=webos-account&profile=<scope>`. Use the
stable browser profile scope issued for the signed-in account. Desktop owns
authentication, Storage, document activation, recovery, and the save picker.
The application window owns one native drawing and its edit history. Keep
the existing standalone entry point for local open, downloads, and recovery.

Register the `.vectorcraft` association when qualifying the mounted app.
Take blank project settings from VectorCraft's New action. Open from Files and
VectorCraft's own Open or Recent through the same Desktop activation path.
The account bridge accepts native projects, including compressed native
projects. File Place and SVG copy retain their existing editor paths. Check the
portability of externally linked image previews before promising that linked
projects travel with their source images. The initial acceptance drawing uses
an embedded image.

## Desktop adapter

The browser bridge uses the same message layout as the PhotoCraft account
adapter with the prefix `vectorcraft:account-`. Check both the exact source
iframe and same origin. Latch the random `generation` from `ready`, and
require that generation on later packets. Require an active account context
before sending requests or accepting writes. Structured-clone payloads
`bytes` hold transferable `ArrayBuffer` values capped at 128 MiB. Document IDs,
editor revisions, write tickets and native dialog IDs are decimal strings.
Parent RPC requests and leases use UUID strings.

| Parent exchange | Fields and results |
|---|---|
| `start` → `started` | Send `requestId`, `path` and `bytes`, with `null` for a blank project. Optional `newSettings` is a JSON string for a blank project. Set `recover` with native draft bytes for explicit restoration. Successful reply carries `ok`, `documentId`, `revision`, and `path`. |
| `dialog` → `dialog-result` | The editor asks for `new`, `open`, or `save`. A new request carries `settings`. An open request can carry an account-relative `path`. A save request carries `suggested`. Answer the exact native `requestId` with `opened`, `cancelled`, `failed` with `message`, or `destination` with `path` for a save. |
| `save` → `save-result` → `save-settled` | All sides echo `ticket`, `documentId`, `revision`, and `path`. Write the supplied native bytes through Storage using the expected server revision or a conditional create for a new destination. Reply `written` only for the verified matching receipt. A confirmed failure uses `failed`. An indeterminate write uses `unknown` without an immediate settlement and requires reconciliation before another write. Final settlement contains `adopted` and `projectState`. |
| `recovery-capture` → `recovery-captured` | Send `requestId`, `documentId`, and `revision` from the last available `project-state`. Store the returned native bytes in Desktop's ordered account-scoped journal. Use `revision` as the recovery version. |
| `autosave` → `autosave-result` | Send `requestId`, `documentId`, `revision`, and the current `path`. The app confirms eligibility and returns `outcome: captured` plus the save tuple and bytes. Process this capture with the same Storage receipt flow. A newer edit or temporary input on the same project returns `busy` for rescheduling. |
| `session-save` → `session-save-result` | Same capture as autosave for an explicitly chosen destination. The reply includes `reviewedRevision` and the precise captured revision. |
| `operation` → `operation-result` | Require the active grant's instance, session and exact resource in Desktop. Send `requestId`, `documentId`, `path` (nullable), `command`, and JSON string `args`. Operations are `document.inspect` with an empty object and `layer.rename` with `layerId` (decimal string) and `name`. Rename uses the editor's `layer.setProps` command and normal Undo. |
| `session` → `session-result` | `pause` sends `requestId`, `lease`, and the exact reviewed `documentId`, `revision`, `dirty`, and `unfinishedEdit` state. After pause, `release` resumes a cancelled transaction and `finalize` ends it. Check `operation`, `lease`, and `ok` on the reply. |

`project-state` exposes `available`, plus `documentId`, `revision`, `dirty`,
`unfinishedEdit`, `operationPending`, `capturable`, and `autosaveEligible`
for an established drawing. Recovery capture and web autosave use these
fields and separate schedules. Account Save As adopts the new path and
marks only the captured edit revision saved after a matching Storage
receipt. Later edits stay dirty. Use confirmation and revision handling
from the shared Desktop adapter for destination collisions.

Route `close-intent` from VectorCraft's File → Close to Desktop's normal
window-close action. For a dirty close or sign-out, let Desktop review
Save, Discard or Cancel against the exact state, then acquire the same
session lease used for sign-out. Save while the app can still encode, and
wait for the receipt. Use `finalize` when all participating reviews have
committed and `release` on cancellation. Keep any explicit discard decision
tied to the reviewed revision and remove the corresponding draft at
commit. The optional `close` → `close-result` request only approves a clean
and settled window. Sending `interactive: true` with that request finalizes
its clean window. A noninteractive check leaves the drawing usable.

## Build and acceptance

Use the existing `packaging/web/package.sh` and Trunk configuration in
`apps/vectorcraft-web/Trunk.toml`. Upstream's release font source is
`storytold/craft-fonts` commit
`b076417b5880bd4154bd22f00695b1d28c6bea83`. Check installed paths against
`crates/text/web-fonts.txt` and the browser catalogue. This font source
contains all 13 manifest faces, including the three on-load requests for
Noto Sans Arabic, Thai, and Georgian Regular. The 30 optional background
Arabic face requests are absent from that upstream font pin. Verify which
font requests the build serves and inspect the actual text after reopening
the acceptance drawing. Check platform asset attribution before release.

The mounted acceptance pass uses a layered drawing with an edited path,
fill, text and embedded image. Test account Save and Save As, duplicate
activation from Files and Open, local draft recovery, reopen from Files in
a fresh browser, confirmed revision, and SVG copy without changing the
native binding. Exercise the documented operation and Undo against the
correct view, and confirm that a grant for a second view or a stale
resource cannot edit the first view. Check a failed/conflicted write and
an unconfirmed response, and verify that a pending gesture, typed edit,
or file read receives the proper close or recovery handling.
