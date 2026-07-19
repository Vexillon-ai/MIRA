Produce a **real, professional data chart** — never ASCII art, never `image_generate`.

Use the **`code_run`** tool with **matplotlib** and save a PNG to `/tmp/output/`; it
renders inline in chat. matplotlib / numpy / pandas are available (missing wheels are
fetched on demand). If `code_run` reports the chart backend isn't available, relay
that and offer to retry once it is — do not fall back to ASCII.

Write clean matplotlib that looks like a polished business chart:

- **Pick the right chart** for the data: pie/donut for parts-of-a-whole, bar for
  category comparison, grouped/stacked bar for multi-series, line for trends over
  time, scatter for correlation, histogram for distributions.
- **Size + resolution:** `figure(figsize=(8,5))`, `savefig(..., dpi=150,
  bbox_inches="tight")` so it's crisp and nothing is clipped.
- **Color:** a coherent, colorful palette (e.g. `plt.get_cmap("tab10"/"Set2")` or an
  explicit hex list). Avoid default muddy grey-on-grey.
- **Labels:** a clear **title**, axis labels with units, a **legend** when there's
  more than one series, and **value/percentage labels** on the data (e.g.
  `autopct="%1.1f%%"` on pies, `bar_label` on bars).
- **Readability:** thousands separators on big numbers, rotated x-tick labels if they
  collide, light gridlines for bar/line charts, no chartjunk.
- **Pie specifics:** order slices largest→smallest, `startangle=90`, a slight
  `explode` on the lead slice or a donut (`wedgeprops=dict(width=0.4)`) for a modern
  look; keep it flat 2-D (matplotlib has no true 3-D pie — a flat pie reads more
  accurately than a faked 3-D one).
- After saving, show the user the `![alt](/api/artifacts/<sha>.<ext>)` markdown
  exactly as `code_run` prints it.

Confirm the numbers/labels match the user's data before finalizing. If the user gave
raw data, use it verbatim; if they asked for well-known figures, state your source
values briefly so they can sanity-check.
