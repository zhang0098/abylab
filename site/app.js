// abylab.ai — the entire client-side script: a theme toggle and copy buttons.
// The stored choice (set here, applied by the inline snippet in <head>) wins
// over Pico's prefers-color-scheme default.
(() => {
  const root = document.documentElement;

  const toggle = document.getElementById("theme-toggle");
  if (toggle) {
    toggle.addEventListener("click", () => {
      const isDark = root.dataset.theme
        ? root.dataset.theme === "dark"
        : window.matchMedia("(prefers-color-scheme: dark)").matches;
      root.dataset.theme = isDark ? "light" : "dark";
      try {
        localStorage.setItem("theme", root.dataset.theme);
      } catch (e) {
        /* private mode: the toggle still works, it just will not stick */
      }
    });
  }

  for (const button of document.querySelectorAll("[data-copy]")) {
    button.addEventListener("click", async () => {
      const source = document.querySelector(button.dataset.copy);
      if (!source) return;
      try {
        await navigator.clipboard.writeText(source.textContent.trim());
      } catch (e) {
        return; // clipboard unavailable (insecure origin, denied permission)
      }
      const label = button.textContent;
      button.textContent = button.dataset.copied || "Copied";
      setTimeout(() => {
        button.textContent = label;
      }, 1400);
    });
  }
})();
