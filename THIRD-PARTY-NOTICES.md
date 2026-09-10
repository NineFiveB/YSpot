# Third-party notices

YSpot includes material from the projects below. Each licence is reproduced in
full, which is what those licences require.

---

## Fluent UI System Icons

SVG path data for the icons drawn beside result rows is vendored from
Microsoft's Fluent UI System Icons. The drawings are used unmodified except for
the removal of upstream's hardcoded `fill` attribute, so that they inherit the
surrounding text colour and respond to Windows high-contrast mode.

- Project: https://github.com/microsoft/fluentui-system-icons
- Vendored at commit: `5bae3fb7771054c252a54b1d9210e9c03439fa1b`
- Where: `apps/shell/src/lib/fluentGlyphs.ts`

```
MIT License

Copyright (c) 2020 Microsoft Corporation

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

**Not included, deliberately:** the Segoe Fluent Icons and Segoe MDL2 Assets
fonts that ship with Windows. YSpot neither redistributes those fonts nor
traces their glyphs into vector paths. Microsoft's font redistribution guidance
forbids converting Windows fonts to other formats, and the fonts themselves are
covered by a separate, revocable assets agreement. The reasoning is recorded in
`docs/M1.md`.
