# Project configuration

A project can keep a `tcode.json` file directly in the project root registered
with Tcode. It belongs to the project on the attached host, including when the
client is on another device. Tcode does not search parent directories or use a
thread's worktree directory instead of that root. This file is separate from
Tcode's application settings and session index.

For example, commit this alongside an image in the repository:

```json
{
  "iconPath": "assets/logo.png"
}
```

## File selection and JSON

Tcode reads `tcode.json` first. Only if that file is absent does it read
`t3.json` in the same directory, using the same rules. The files are never
merged. An existing but empty, unreadable, oversized or malformed `tcode.json`
does not fall back to `t3.json`. Neither does a primary file with no icon, or
one pointing to an invalid image. If both files are absent, the project has
no configured default icon.

The file must be a JSON object, at most 1 MiB. Standard JSON applies: comments
and trailing commas are not supported. Unknown fields are ignored, including
`scripts`; Tcode does not execute anything from this file. Invalid JSON or a
known field with the wrong type makes the configuration invalid. A bad
configuration does not prevent the project or its threads from opening; its
icon falls back to the folder glyph unless a manual icon overrides it.

The reader and schema live in
[`crates/services/src/project_config.rs`](../crates/services/src/project_config.rs).

## `iconPath`

`iconPath` is an optional string. An absent field, `null`, an empty string or a
whitespace-only string means no configured icon. Any other string is used as
written; leading or trailing spaces are part of the filename.

Relative paths resolve against the registered project root. Absolute paths are
accepted according to the host operating system's path rules. For example,
`assets/logo.png` refers to that file inside the project. `../shared/logo.png`
can refer outside it. `~`, environment variables and URLs are not expanded.
Prefer a relative path to a checked-in image when sharing a project across
machines. Windows paths in JSON need escaped backslashes, for example
`"C:\\Artwork\\logo.png"`.

Supported image formats are PNG, JPEG, WebP, GIF, BMP, TIFF and ICO. Images must
be at most 8 MiB, at most 8192 pixels on either side, and at most 4,194,304 pixels
in total. The decoder also has a 128 MiB allocation limit; the pixel limit
bounds the additional floating-point resize buffers. Animated images use a
static frame. A missing, unreadable, unsupported or invalid image uses the
folder glyph.

## Manual icons and refresh

**Change project icon** in the project menu, or **Change icon for ‹project›** in
the command palette, opens a picker for the attached host's files. Selecting an
image creates a static PNG copy of at most 128 × 128 pixels in the host's
Tcode data directory (`project-icons/`). The session index records that copy;
Tcode does not modify the original image or either project config file.
Removing the original therefore does not break the manual icon. Replacing,
resetting or removing the project cleans up the previous managed copy after
the updated index is persisted.

A manual selection takes precedence over both config files, even if the config
is invalid. If the managed image itself becomes unavailable, the folder glyph
is shown. **Use project default** clears the manual choice and reloads the
configuration, including on other connected clients. It can also refresh an
already selected default after editing its config or image.

Defaults load when displayed, and refresh on reset, reconnect, or client
restart. Tcode does not watch config or image files for live changes.

## Future configuration management

Tcode currently reads these files; it does not generate, migrate or edit them.
The standalone configuration reader is the owner for future project settings,
so new consumers do not invent different fallback or parsing rules.

A future configuration editor should write the canonical `tcode.json`, preserve
unknown fields, validate known fields before saving, and replace the file
atomically. It must report invalid existing JSON rather than overwriting it
with an empty configuration. Creating a primary file shadows the entire legacy
file, so migration from `t3.json` must preserve its other fields deliberately.
These are requirements for future management, not a currently available editor.
