# Third-party notices

## SubCraft

Selected source is adapted from [DrapNard/subcraft](https://github.com/DrapNard/subcraft),
a fork of [Aalzard/subcraft](https://github.com/Aalzard/subcraft), at commit
`71e49fde7e4e1dba188a6589c9e5a139d1e3d9d5`.

Adapted areas: compositor blend math and color helpers, viewport shading,
curvature neighbourhood evaluation and inward-ray thickness baking. Source
headers identify adaptations; interfaces, storage, limits and app integration
follow Kalium. See [integration record](docs/architecture/subcraft-integration.md).

Upstream offers MIT OR Apache-2.0. These adaptations use the MIT option.
Kalium's combined distribution remains AGPL-3.0-only; this notice and upstream
permissions remain applicable to the incorporated portions. Both upstream
license files are retained in `third_party/subcraft/`.

### MIT notice

Copyright (c) Aalzard and the SubCraft contributors

Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the
Software without restriction, including without
limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software
is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice
shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
