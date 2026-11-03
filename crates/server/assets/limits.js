// Makes the usage-limit bars draggable. Each bar edits the two fields named
// by its data-warn and data-stop, and redraws its green, amber and red as the
// marks move; the fields stay the source of truth, so saving works as before.
(() => {
  const clamp = (n, lo, hi) => Math.min(hi, Math.max(lo, n));

  const init = (range) => {
    const warnField = document.getElementById(range.dataset.warn);
    const stopField = document.getElementById(range.dataset.stop);
    const rail = range.querySelector(".rail");
    if (!warnField || !stopField || !rail) return;
    range.classList.add("live");
    let stop = 100;
    let warn = [];
    let marks = [];

    // Levels from the fields, kept in range the way the server will read them.
    const read = () => {
      stop = clamp(parseInt(stopField.value, 10) || 100, 1, 100);
      warn = [...new Set(warnField.value.split(",").map((t) => parseInt(t, 10))
        .filter((n) => n >= 1 && n < stop))].sort((a, b) => a - b);
    };
    const write = () => {
      warnField.value = warn.join(", ");
      stopField.value = stop;
    };
    const bounds = (mark) => {
      if (mark.kind === "stop") return [warn.length ? warn[warn.length - 1] + 1 : 1, 100];
      const i = mark.index;
      return [i > 0 ? warn[i - 1] + 1 : 1, i < warn.length - 1 ? warn[i + 1] - 1 : stop - 1];
    };
    const valueOf = (mark) => (mark.kind === "stop" ? stop : warn[mark.index]);
    const setValue = (mark, v) => {
      if (mark.kind === "stop") stop = v; else warn[mark.index] = v;
    };

    // Moves the marks and recolors the bar without rebuilding them, so a mark
    // being dragged keeps the pointer.
    const place = () => {
      range.style.setProperty("--w", (warn.length ? warn[0] : stop) + "%");
      range.style.setProperty("--s", stop + "%");
      for (const mark of marks) {
        const v = valueOf(mark);
        const [lo, hi] = bounds(mark);
        mark.el.style.left = v + "%";
        mark.el.firstChild.textContent = mark.kind + " " + v + "%";
        mark.el.setAttribute("aria-valuenow", v);
        mark.el.setAttribute("aria-valuetext", v + "%");
        mark.el.setAttribute("aria-valuemin", lo);
        mark.el.setAttribute("aria-valuemax", hi);
      }
    };

    const move = (mark, v) => {
      const [lo, hi] = bounds(mark);
      if (hi < lo) return;
      setValue(mark, clamp(Math.round(v), lo, hi));
      place();
      write();
    };
    const percentAt = (x) => {
      const box = rail.getBoundingClientRect();
      return ((x - box.left) / box.width) * 100;
    };

    const build = () => {
      for (const mark of marks) mark.el.remove();
      marks = [...warn.map((_, index) => ({ kind: "warn", index })), { kind: "stop" }];
      for (const mark of marks) {
        const el = document.createElement("button");
        el.type = "button";
        el.className = "mk " + mark.kind;
        el.setAttribute("role", "slider");
        el.setAttribute("aria-label", mark.kind === "stop" ? "Stop level" : "Warning level " + (mark.index + 1));
        el.appendChild(document.createElement("span"));
        mark.el = el;
        el.addEventListener("pointerdown", (e) => {
          e.preventDefault();
          el.setPointerCapture(e.pointerId);
          el.classList.add("held");
        });
        el.addEventListener("pointermove", (e) => {
          if (el.hasPointerCapture(e.pointerId)) move(mark, percentAt(e.clientX));
        });
        const release = (e) => {
          el.classList.remove("held");
          if (el.hasPointerCapture(e.pointerId)) el.releasePointerCapture(e.pointerId);
        };
        el.addEventListener("pointerup", release);
        el.addEventListener("pointercancel", release);
        el.addEventListener("keydown", (e) => {
          const step = e.shiftKey ? 5 : 1;
          const [lo, hi] = bounds(mark);
          const keys = { ArrowLeft: valueOf(mark) - step, ArrowDown: valueOf(mark) - step,
            ArrowRight: valueOf(mark) + step, ArrowUp: valueOf(mark) + step, Home: lo, End: hi };
          if (e.key in keys) {
            e.preventDefault();
            move(mark, keys[e.key]);
          } else if ((e.key === "Delete" || e.key === "Backspace") && mark.kind === "warn") {
            e.preventDefault();
            removeWarn(mark.index);
          }
        });
        el.addEventListener("dblclick", () => { if (mark.kind === "warn") removeWarn(mark.index); });
        range.appendChild(el);
      }
      place();
    };
    const removeWarn = (index) => {
      warn.splice(index, 1);
      write();
      build();
    };

    // A double-click on the bar adds a warning level there.
    range.addEventListener("dblclick", (e) => {
      if (e.target.closest(".mk")) return;
      const v = Math.round(percentAt(e.clientX));
      if (v < 1 || v >= stop || warn.includes(v)) return;
      warn = [...warn, v].sort((a, b) => a - b);
      write();
      build();
    });
    // Typing in a field moves the marks to match.
    const refresh = () => { read(); build(); };
    warnField.addEventListener("input", refresh);
    stopField.addEventListener("input", refresh);

    range.querySelectorAll(".mk").forEach((el) => el.remove());
    refresh();
    const hint = document.createElement("p");
    hint.className = "hint";
    hint.textContent = "Drag a mark to move it. Double-click the bar to add a warning level, or a warning mark to remove it.";
    range.after(hint);
  };

  document.querySelectorAll(".range[data-warn][data-stop]").forEach(init);
})();
