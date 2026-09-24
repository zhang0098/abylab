// abylab.ai — the entire client-side script: a theme toggle and copy buttons.
// The stored choice (set here, applied by the inline snippet in <head>) wins
// over Pico's prefers-color-scheme default.
(() => {
  const root = document.documentElement;
  const preferredTheme = window.matchMedia("(prefers-color-scheme: dark)");
  const isDark = () => root.dataset.theme
    ? root.dataset.theme === "dark"
    : preferredTheme.matches;

  const toggle = document.getElementById("theme-toggle");
  const updateTheme = () => {
    const dark = isDark();
    if (toggle) {
      toggle.setAttribute("aria-label", dark ? toggle.dataset.labelLight : toggle.dataset.labelDark);
      toggle.setAttribute("aria-pressed", String(dark));
    }
    const color = document.querySelector('meta[name="theme-color"]');
    if (color) color.content = dark ? "#151c18" : "#f6f7f2";
  };
  updateTheme();
  preferredTheme.addEventListener("change", updateTheme);

  if (toggle) {
    toggle.addEventListener("click", () => {
      root.dataset.theme = isDark() ? "light" : "dark";
      updateTheme();
      try {
        localStorage.setItem("theme", root.dataset.theme);
      } catch (e) {
        /* private mode: the toggle still works, it just will not stick */
      }
    });
  }

  const status = document.getElementById("copy-status");
  for (const button of document.querySelectorAll("[data-copy]")) {
    const label = button.textContent;
    let resetTimer;
    button.addEventListener("click", async () => {
      const source = document.querySelector(button.dataset.copy);
      if (!source) return;
      clearTimeout(resetTimer);
      if (status) status.textContent = "";
      try {
        await navigator.clipboard.writeText(source.textContent.trim());
        button.textContent = button.dataset.copied || "Copied";
      } catch (e) {
        button.textContent = button.dataset.copyError || "Copy manually";
      }
      if (status) status.textContent = button.textContent;
      resetTimer = setTimeout(() => {
        button.textContent = label;
      }, 1800);
    });
  }
})();
