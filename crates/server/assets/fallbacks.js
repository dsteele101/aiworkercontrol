(() => {
  const list = document.getElementById("fallbacks");
  const form = list.closest("form");
  const add = document.getElementById("add-fallback");
  let dragged = null;
  // A provider can be listed once: disable each option another row has taken.
  const refresh = () => {
    const taken = [...list.querySelectorAll("select")].map((s) => s.value);
    list.querySelectorAll("select").forEach((s) => {
      s.querySelectorAll("option").forEach((o) => {
        o.disabled = o.value !== s.value && taken.includes(o.value);
      });
    });
    add.disabled = [...template().querySelectorAll("option")].every((o) => taken.includes(o.value));
  };
  const template = () => document.getElementById("fallback-template").content;
  const wire = (row) => {
    row.querySelector(".remove").onclick = () => { row.remove(); refresh(); };
    row.querySelector("select").onchange = refresh;
    row.ondragstart = () => { dragged = row; row.classList.add("dragging"); };
    row.ondragend = () => { dragged = null; row.classList.remove("dragging"); };
    row.ondragover = (e) => {
      e.preventDefault();
      if (!dragged || dragged === row) return;
      const after = e.clientY > row.getBoundingClientRect().top + row.offsetHeight / 2;
      list.insertBefore(dragged, after ? row.nextSibling : row);
    };
  };
  list.querySelectorAll("li").forEach(wire);
  add.onclick = () => {
    const row = template().firstElementChild.cloneNode(true);
    const taken = [...list.querySelectorAll("select")].map((s) => s.value);
    const free = [...row.querySelectorAll("option")].find((o) => !taken.includes(o.value));
    if (!free) return;
    row.querySelector("select").value = free.value;
    list.appendChild(row);
    wire(row);
    refresh();
  };
  refresh();
  form.addEventListener("submit", () => {
    list.querySelectorAll("select").forEach((s, i) => { s.name = "fallback" + (i + 1); });
  });
})();
