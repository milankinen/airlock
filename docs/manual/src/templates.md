# Templates

Airlock installs the [packs](./packs.md) of a sandbox separately for each
sandbox. With many packs, this takes a long time. If you use the same
config again and again, create a template. A new sandbox from a template
starts immediately with the packs of the template installed.

## Commands

```bash
airlock templates list              # All templates
airlock templates info <name>       # Details of one template
airlock templates create <name>     # Create a new template
airlock templates remove <name>     # Remove a template
```

`list` shows a table with these columns:

| Column  | Meaning                                         |
|---------|-------------------------------------------------|
| `NAME`  | Template name                                   |
| `IMAGE` | Image name with a short digest                  |
| `DISK`  | Disk space that the template uses               |
| `PACKS` | Packs on the template disk                      |

`info` shows the name, the directory, the creation time, the image, the
disk use and size, the clipboard setting, and the packs with their
arguments.

## Creating a template

```bash
airlock templates create my-base
```

This opens the setup wizard. Select the packs to install in the template.
Then airlock installs them and saves the template.

The template name has 1 to 64 characters. Use letters, digits, `-`, `_`
and `.`. The first character is a letter or a digit.

Options:

| Option              | Meaning                    |
|---------------------|----------------------------|
| `--log-level <L>`   | Set the log level          |
| `-v`                | Show more output           |

## Using a template

The templates that you create show in the setup wizard of
[`airlock start`](./usage/starting-sandbox.md). Select a template for the
new sandbox. You can also select more packs for the sandbox.

## Removing a template

```bash
airlock templates remove my-base
airlock templates remove -f my-base   # No confirmation
```

The command asks for confirmation. Pass `-f` to skip the question.

Sandboxes that you made from the template continue to work. They have
their own disk.

## Security considerations

- Sandboxes from the same template use the same CA certificate for the
  airlock TLS interception.
