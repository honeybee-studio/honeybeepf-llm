// Prints simulated calls on the demo workloads onto the home page receipt.
// Stays still when the reader prefers reduced motion or the tab is hidden.
(function () {
  const WORKLOADS = [
    { team: "team-a", name: "team-a/chat-api", weight: 4, models: [["gpt-5.4-mini", 0.0009]] },
    { team: "team-b", name: "team-b/support-bot", weight: 3, models: [["claude-sonnet-4-5", 0.006], ["claude-opus-5-5", 0.02]] },
    { team: "team-c", name: "team-c/summarizer", weight: 2, models: [["gemini-2.5-flash", 0.0004]] },
  ];
  const TOTAL_WEIGHT = WORKLOADS.reduce((sum, w) => sum + w.weight, 0);
  const SHOWN = 4;
  let timer;

  function pick() {
    let r = Math.random() * TOTAL_WEIGHT;
    for (const w of WORKLOADS) {
      r -= w.weight;
      if (r < 0) return w;
    }
    return WORKLOADS[0];
  }

  function cell(tag, text) {
    const el = document.createElement(tag);
    el.textContent = text;
    return el;
  }

  function start() {
    clearInterval(timer);
    const receipt = document.querySelector("[data-hb-live]");
    if (!receipt || window.matchMedia("(prefers-reduced-motion: reduce)").matches) return;

    const teams = new Map();
    receipt.querySelectorAll("[data-hb-team]").forEach((line) => {
      teams.set(line.dataset.hbTeam, { line, usd: parseFloat(line.dataset.hbUsd) });
    });
    const items = receipt.querySelector("[data-hb-log]");
    const total = receipt.querySelector("[data-hb-total]");

    function print() {
      const w = pick();
      const [model, perThousand] = w.models[Math.floor(Math.random() * w.models.length)];
      const tokens = 400 + Math.floor(Math.random() * 2800);
      const cost = (tokens / 1000) * perThousand;

      const li = document.createElement("li");
      li.append(cell("b", w.name), cell("span", model + " × " + tokens.toLocaleString("en-US") + " tok"),
        cell("em", "$" + cost.toFixed(3)));
      items.prepend(li);
      while (items.children.length > SHOWN) items.lastElementChild.remove();

      const t = teams.get(w.team);
      t.usd += cost;
      t.line.querySelector(".hb-amount").textContent = "$" + t.usd.toFixed(2);
      t.line.classList.remove("hb-flash");
      void t.line.offsetWidth;
      t.line.classList.add("hb-flash");
      let sum = 0;
      teams.forEach((x) => (sum += x.usd));
      total.textContent = "$" + sum.toFixed(2);
    }

    timer = setInterval(() => {
      if (!document.hidden) print();
    }, 1600);
  }

  // Material's instant navigation swaps pages without a reload and announces each one on document$.
  if (window.document$) window.document$.subscribe(start);
  else document.addEventListener("DOMContentLoaded", start);
})();
