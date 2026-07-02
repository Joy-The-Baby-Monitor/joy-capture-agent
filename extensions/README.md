# Joy extensions

First-party Python extensions (examples and bundled detectors/behaviors) live
here. Each extension ships a `joy.toml` manifest declaring its id/namespace,
version, subscribed inputs, emitted event kinds, and Python dependencies. The
extension host (`joy-ext`) reads the manifest, subscribes the extension only to
what it asked for, and routes emitted `ext.<vendor>.*` events back onto the bus.

Empty for now — the contract and the first example extension land during the
extension-host milestone.
