// Passession's script for an `ask` form shown in a web view. The app injects it; the page's own
// scripts never run (a CSP in `wrap_html` and each engine's settings block them). Native
// engines evaluate it in the page; the browser build evaluates it in the app's page against
// the sandboxed iframe's document. `paseoInit(doc, post, config)` returns `{submit}`.
//
// Messages to the app (`post`): {type:"height", value} with the content height in CSS px;
// {type:"submitted", data:{values:{name:[value...]}, other:{name:text}}}; {type:"invalid"};
// {type:"scroll", dx, dy} for a wheel turn the page does not use, in CSS px.
(function () {
    // The value of an injected Other box; never a value the form can submit itself.
    const OTHER = "\u0000paseo-other";

    function init(doc, post, config) {
        const root = doc.documentElement;
        for (const [name, value] of Object.entries(config.theme)) {
            root.style.setProperty(name, value);
        }
        root.style.colorScheme = config.dark ? "dark" : "light";
        const form = doc.querySelector("form");

        // OMP adds a free-text Other answer to radio and checkbox groups; so does the card.
        for (const name of config.other) {
            const group = [...doc.querySelectorAll("input[name]")].filter(
                (input) => input.name === name && (input.type === "radio" || input.type === "checkbox"),
            );
            if (group.length === 0) continue;
            const last = group[group.length - 1];
            const row = doc.createElement("label");
            row.className = "paseo-other";
            const box = doc.createElement("input");
            box.type = last.type;
            box.name = name;
            box.value = OTHER;
            if (last.hasAttribute("form")) box.setAttribute("form", last.getAttribute("form"));
            const text = doc.createElement("input");
            text.type = "text";
            text.placeholder = config.otherLabel;
            text.dataset.paseoOther = name;
            text.addEventListener("input", () => {
                box.checked = text.value.trim() !== "";
            });
            row.append(box, " ", text);
            (last.closest("label") || last).after(row);
        }

        function submit() {
            if (form && !form.reportValidity()) {
                post({ type: "invalid" });
                return;
            }
            const values = {};
            const other = {};
            const entries = form ? new FormData(form) : [];
            for (const [name, value] of entries) {
                if (typeof value !== "string") continue;
                if (value === OTHER) {
                    const text = [...doc.querySelectorAll("input[data-paseo-other]")].find(
                        (input) => input.dataset.paseoOther === name,
                    );
                    other[name] = text ? text.value : "";
                    continue;
                }
                (values[name] = values[name] || []).push(value);
            }
            post({ type: "submitted", data: { values, other } });
        }

        // Enter in a one-line field submits, as in the card. Implicit form submission is not
        // relied on: a sandboxed frame without `allow-forms` never fires `submit`.
        doc.addEventListener(
            "keydown",
            (event) => {
                const target = event.target;
                if (
                    event.key === "Enter" &&
                    !event.isComposing &&
                    target instanceof doc.defaultView.HTMLInputElement &&
                    !["checkbox", "radio", "button", "submit", "reset", "file"].includes(target.type)
                ) {
                    event.preventDefault();
                    submit();
                }
            },
            true,
        );
        if (form) {
            form.addEventListener("submit", (event) => {
                event.preventDefault();
                submit();
            });
        }

        // The page is as tall as its content and never scrolls: the wheel scrolls the
        // conversation around it, except over an element that can still scroll that way.
        const scrolls = (element, dy) => {
            for (let node = element; node && node !== doc.body; node = node.parentElement) {
                if (node.scrollHeight <= node.clientHeight) continue;
                if (dy < 0 ? node.scrollTop > 0 : node.scrollTop + node.clientHeight < node.scrollHeight) {
                    return true;
                }
            }
            return false;
        };
        doc.addEventListener(
            "wheel",
            (event) => {
                if (event.ctrlKey || scrolls(event.target, event.deltaY)) return;
                event.preventDefault();
                const unit =
                    event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? doc.defaultView.innerHeight : 1;
                post({ type: "scroll", dx: event.deltaX * unit, dy: event.deltaY * unit });
            },
            { passive: false },
        );

        let reported = -1;
        const measure = () => {
            const body = doc.body;
            const style = doc.defaultView.getComputedStyle(body);
            const height = Math.ceil(
                body.getBoundingClientRect().bottom + parseFloat(style.marginBottom || "0"),
            );
            if (height !== reported) {
                reported = height;
                post({ type: "height", value: height });
            }
        };
        new doc.defaultView.ResizeObserver(measure).observe(doc.body);
        measure();
        return { submit };
    }

    window.paseoInit = init;
})();
