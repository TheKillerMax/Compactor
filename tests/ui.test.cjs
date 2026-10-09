"use strict";

// Unit tests execute the shipped ES5 application, including its real event
// handlers and bridge messages. This small DOM adapter is not a webview/layout
// emulator; Windows UI and WOF integration are covered separately.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const html = fs.readFileSync(path.join(__dirname, "../src/ui/index.html"), "utf8");
const script = fs.readFileSync(path.join(__dirname, "../src/ui/app.js"), "utf8");

class Element {
    constructor(tag, attributes = {}) {
        this.tag = tag.toLowerCase();
        this.attributes = attributes;
        this.parent = null;
        this.children = [];
        this.content = "";
        this.value = attributes.value || "";
        this.checked = Object.hasOwn(attributes, "checked");
        this.disabled = Object.hasOwn(attributes, "disabled");
        this.style = { display: /display\s*:\s*none/.test(attributes.style || "") ? "none" : "" };
        this.classes = new Set((attributes.class || "").split(/\s+/).filter(Boolean));
        this.handlers = new Map();
    }

    append(child) {
        if (!(child instanceof Element)) {
            const text = new Element("#text");
            text.content = String(child);
            child = text;
        }
        child.parent = this;
        this.children.push(child);
    }

    get text() {
        return this.content + this.children.map(child => child.text).join("");
    }

    get visible() {
        return this.style.display !== "none";
    }
}

function parseDocument(source) {
    const root = new Element("document");
    const stack = [root];
    const tags = /<\/?([a-z][a-z0-9-]*)([^>]*)>/gi;
    const voidTags = new Set(["meta", "input", "br", "hr", "img", "link"]);
    let previous = 0;
    let match;
    while ((match = tags.exec(source))) {
        stack[stack.length - 1].content += source.slice(previous, match.index);
        previous = tags.lastIndex;
        const name = match[1].toLowerCase();
        if (match[0].startsWith("</")) {
            while (stack.length > 1 && stack[stack.length - 1].tag !== name) stack.pop();
            if (stack.length > 1) stack.pop();
            continue;
        }
        const attrs = {};
        for (const attr of match[2].matchAll(/([a-z][a-z0-9_-]*)(?:="([^"]*)")?/gi)) {
            attrs[attr[1]] = attr[2] === undefined ? "" : attr[2];
        }
        const element = new Element(name, attrs);
        stack[stack.length - 1].append(element);
        if (!voidTags.has(name)) stack.push(element);
    }
    return root;
}

function descendants(root) {
    return root.children.flatMap(child => [child, ...descendants(child)]);
}

function matchesSimple(element, selector) {
    const parsed = /^([a-z]+)?(?:#([\w-]+))?(?:\.([\w-]+))?(?:\[([\w-]+)\])?$/i.exec(selector);
    if (!parsed) throw new Error("Unsupported test selector: " + selector);
    return (!parsed[1] || element.tag === parsed[1].toLowerCase()) &&
        (!parsed[2] || element.attributes.id === parsed[2]) &&
        (!parsed[3] || element.classes.has(parsed[3])) &&
        (!parsed[4] || Object.hasOwn(element.attributes, parsed[4]));
}

function matches(element, selector) {
    const parts = selector.trim().split(/\s+/);
    if (!matchesSimple(element, parts.pop())) return false;
    let parent = element.parent;
    while (parts.length) {
        const part = parts.pop();
        while (parent && !matchesSimple(parent, part)) parent = parent.parent;
        if (!parent) return false;
        parent = parent.parent;
    }
    return true;
}

function application() {
    const root = parseDocument(html);
    const elements = descendants(root);
    const ids = new Map();
    for (const element of elements) {
        if (element.attributes.id) {
            assert.ok(!ids.has(element.attributes.id), "Duplicate HTML id: " + element.attributes.id);
            ids.set(element.attributes.id, element);
        }
        if (element.tag === "select") {
            const options = descendants(element).filter(child => child.tag === "option");
            element.value = (options.find(option => Object.hasOwn(option.attributes, "selected")) || options[0])?.attributes.value || "";
        }
    }

    class Selection {
        constructor(selected) { this.nodes = selected; }
        each(callback) { this.nodes.forEach(callback); return this; }
        val(value) {
            if (arguments.length === 0) return this.nodes[0]?.value;
            return this.each(node => {
                const string = String(value);
                node.value = node.tag === "select" && !node.children.some(child => child.attributes.value === string) ? "" : string;
            });
        }
        text(value) {
            if (arguments.length === 0) return this.nodes.map(node => node.text).join("");
            return this.each(node => { node.content = String(value); node.children = []; });
        }
        prop(name, value) {
            if (arguments.length === 1) return this.nodes[0]?.[name];
            return this.each(node => { node[name] = value; });
        }
        attr(name, value) {
            if (arguments.length === 1) return this.nodes[0]?.attributes[name];
            return this.each(node => { node.attributes[name] = String(value); });
        }
        removeAttr(name) { return this.each(node => { delete node.attributes[name]; }); }
        show() { return this.each(node => { node.style.display = ""; }); }
        hide() { return this.each(node => { node.style.display = "none"; }); }
        toggle(shown) { return this.each(node => { node.style.display = shown ? "" : "none"; }); }
        empty() { return this.each(node => { node.content = ""; node.children = []; }); }
        append(value) {
            return this.each(node => {
                const children = value instanceof Selection ? value.nodes : [value];
                children.forEach(child => node.append(child));
            });
        }
        addClass(name) { return this.each(node => { node.classes.add(name); }); }
        removeClass(name) { return this.each(node => { node.classes.delete(name); }); }
        ready(callback) { callback(); return this; }
        on(event, callback) {
            return this.each(node => {
                const handlers = node.handlers.get(event) || [];
                handlers.push(callback);
                node.handlers.set(event, handlers);
            });
        }
        trigger(event) {
            return this.each(node => {
                if (event === "click" && node.disabled) return;
                for (const handler of node.handlers.get(event) || []) handler.call(node, { preventDefault() {} });
            });
        }
    }

    const document = {
        getElementById(id) {
            assert.ok(ids.has(id), "Application references missing HTML id: " + id);
            return ids.get(id);
        },
        createTextNode(text) { const node = new Element("#text"); node.content = text; return node; }
    };
    const $ = selector => {
        if (selector === document) return new Selection([root]);
        if (selector instanceof Element) return new Selection([selector]);
        if (selector.startsWith("<")) return new Selection(parseDocument(selector).children);
        const selectors = selector.split(",");
        return new Selection(elements.filter(element => selectors.some(part => matches(element, part))));
    };
    let now = 0;
    let nextTimer = 0;
    const timers = new Map();
    const requests = [];
    const alerts = [];
    const context = vm.createContext({
        $, document,
        window: { performance: { now: () => now }, alert: message => alerts.push(message) },
        external: { invoke: request => requests.push(JSON.parse(request)) },
        setTimeout(callback, delay) { const id = ++nextTimer; timers.set(id, { callback, due: now + delay }); return id; },
        clearTimeout(id) { timers.delete(id); }
    });
    vm.runInContext(script, context, { filename: "src/ui/app.js" });

    return {
        ...context, $, requests, alerts, timers,
        element: id => document.getElementById(id),
        advance(milliseconds) {
            const end = now + milliseconds;
            let ticks = 0;
            while (timers.size) {
                const [id, timer] = [...timers].sort((a, b) => a[1].due - b[1].due)[0];
                if (timer.due > end) break;
                assert.ok(ticks++ < 1000, "Unexpected timer loop");
                now = timer.due;
                timers.delete(id);
                timer.callback();
            }
            now = end;
        }
    };
}

function config(overrides = {}) {
    return {
        type: "Config", decimal: false, compression: "LZX", analysis_mode: "Sampled",
        min_savings_percent: 1, max_threads: 0, compression_priority: "BelowNormal",
        hdd_single_thread: true, protect_direct_storage: true, excludes: "C:\\Windows*",
        ...overrides
    };
}

function summary() {
    return {
        logical_size: 6000, physical_size: 3000, direct_storage: false,
        compressed: { count: 1, logical_size: 4000, physical_size: 1000, estimated_physical_size: 1000 },
        compressible: { count: 1, logical_size: 1000, physical_size: 1000, estimated_physical_size: 500 },
        skipped: { count: 1, logical_size: 1000, physical_size: 1000, estimated_physical_size: 1000 }
    };
}

function finishScan(app, info = summary()) {
    app.Response.dispatch({ type: "FolderSummary", info });
    app.Response.dispatch({ type: "Scanned" });
}

function compressedView(overrides = {}) {
    return {
        type: "CompressedView", root: "C:\\Games", view: "folders", query: "", page: 0, pages: 1,
        total: 1, compressed_count: 2, logical_size: 8192, physical_size: 4096,
        items: [{ kind: "Folder", count: 2, path: "Game\\Data", logical_size: 8192, physical_size: 4096 }],
        ...overrides
    };
}

test("configuration preserves the four WOF methods and defaults legacy messages to sampled", () => {
    const app = application();
    const legacy = config({ compression: "XPRESS4k" });
    delete legacy.analysis_mode;
    app.Response.dispatch(legacy);
    assert.equal(app.$("#Compression_Mode").val(), "XPRESS4K");
    assert.equal(app.$("#Analysis_Mode").val(), "Sampled");
    assert.deepEqual(app.element("Compression_Mode").children.map(option => option.attributes.value), ["XPRESS4K", "XPRESS8K", "XPRESS16K", "LZX"]);
});

test("saving settings sends the complete typed configuration through the real click handler", () => {
    const app = application();
    app.Response.dispatch(config());
    app.$("#Analysis_Mode").val("Thorough");
    app.$("#Min_Savings").val("2.5");
    app.$("#Max_Threads").val("4");
    app.$("#SI_Units").val("D");
    app.$("#Compression_Priority").val("Normal");
    app.$("#HDD_Single_Thread").prop("checked", false);
    app.$("#Button_Save").trigger("click");
    assert.deepEqual(app.requests, [{
        type: "SaveConfig", decimal: true, compression: "LZX", analysis_mode: "Thorough",
        min_savings_percent: 2.5, max_threads: 4, compression_priority: "Normal",
        hdd_single_thread: false, protect_direct_storage: true, excludes: "C:\\Windows*"
    }]);
});

test("invalid numeric settings are rejected without rounding, defaulting, or sending a malformed request", () => {
    const cases = [
        ...["", " ", "-0.1", "100.1", "NaN", "Infinity", "2 percent", "0x10"].map(value => ["Min_Savings", value]),
        ...["", "-1", "1.9", "17", "1 thread", "1e1000"].map(value => ["Max_Threads", value])
    ];
    for (const [id, value] of cases) {
        const app = application();
        app.Response.dispatch(config());
        app.$("#" + id).val(value);
        app.$("#Button_Save").trigger("click");
        assert.equal(app.requests.length, 0, id + "=" + value);
        assert.equal(app.alerts.length, 1, id + "=" + value);
    }
});

test("valid numeric boundary values remain available", () => {
    for (const [saving, threads] of [[0, 0], [100, 16]]) {
        const app = application();
        app.Response.dispatch(config());
        app.$("#Min_Savings").val(saving);
        app.$("#Max_Threads").val(threads);
        app.$("#Button_Save").trigger("click");
        assert.equal(app.requests[0].min_savings_percent, saving);
        assert.equal(app.requests[0].max_threads, threads);
    }
});

test("active operations lock settings and new operations while keeping pause/resume/stop usable", () => {
    const app = application();
    app.Response.dispatch(config());
    app.Gui.scanning();
    for (const id of ["Button_Folder", "Button_Save", "Button_Reset", "Compression_Mode", "Analysis_Mode", "Excludes", "Min_Savings", "Max_Threads", "SI_Units"]) {
        assert.equal(app.element(id).disabled, true, id);
    }
    app.Action.choose_folder();
    app.Action.analyse();
    app.Action.compress();
    app.Action.decompress();
    app.Action.decompress_and_exclude();
    app.Action.reset_config();
    app.Action.save_config({});
    assert.deepEqual(app.requests, []);
    app.Action.pause();
    app.Response.dispatch({ type: "Paused" });
    assert.equal(app.element("Button_Resume").visible, true);
    app.Action.resume();
    app.Action.stop();
    assert.deepEqual(app.requests.map(request => request.type), ["Pause", "Resume", "Stop"]);
    finishScan(app);
    assert.equal(app.Gui.is_busy(), false);
    assert.equal(app.element("Analysis_Mode").disabled, false);
});

test("requesting an operation locks settings before the backend replies and startup errors release them", () => {
    const app = application();
    app.Response.dispatch(config());
    finishScan(app);
    app.Action.compress();
    assert.equal(app.Gui.is_busy(), true);
    assert.equal(app.element("Button_Save").disabled, true);
    app.Action.save_config({});
    app.Action.compress();
    assert.deepEqual(app.requests, [{ type: "Compress" }]);
    app.Response.dispatch({ type: "Error", title: "Folder not supported", message: "The folder is no longer available." });
    assert.equal(app.Gui.is_busy(), false);
    assert.equal(app.element("Button_Folder").disabled, false);
    assert.equal(app.element("Button_Compress").visible, true);
});

test("cancelling folder selection preserves the previous summary and invalidation state", () => {
    const app = application();
    finishScan(app);
    app.Response.dispatch({ type: "AnalysisInvalidated" });
    const originalSummary = app.$("#Size_Logical").text();
    app.Action.choose_folder();
    assert.equal(app.Gui.is_busy(), true);
    assert.equal(app.$("#Size_Logical").text(), originalSummary);
    app.Response.dispatch({ type: "Scanned" });
    assert.equal(app.Gui.is_busy(), false);
    assert.equal(app.Gui.needs_analysis(), true);
    assert.equal(app.element("Button_Compress").visible, false);
    assert.equal(app.element("Button_Decompress").visible, true);
});

test("a reported error during active work cannot unlock settings before the stopped event", () => {
    const app = application();
    app.Gui.compacting();
    app.Response.dispatch({ type: "Error", title: "Check incomplete", message: "One file could not be checked." });
    assert.equal(app.Gui.is_busy(), true);
    app.Response.dispatch({ type: "Stopped" });
    app.Response.dispatch({ type: "AnalysisInvalidated" });
    assert.equal(app.Gui.is_busy(), false);
    assert.equal(app.Gui.needs_analysis(), true);
});

test("coalesced status and summary updates are applied before the completed state", () => {
    const app = application();
    app.Gui.scanning();
    app.Response.dispatch({ type: "Status", status: "older", pct: 0.1 });
    app.Response.dispatch({ type: "Status", status: "latest", pct: 0.9 });
    finishScan(app);
    assert.equal(app.$("#Activity_Text").text(), "latest");
    assert.equal(app.element("Button_Compress").visible, true);
    assert.equal(app.element("Button_Decompress").visible, true);
    assert.equal(app.timers.size, 0);
});

test("settings invalidation blocks compression until analysis runs again", () => {
    const app = application();
    finishScan(app);
    app.Response.dispatch({ type: "AnalysisInvalidated" });
    assert.equal(app.Gui.needs_analysis(), true);
    assert.equal(app.element("Button_Compress").visible, false);
    assert.equal(app.element("Estimated_Stale").visible, true);
    app.Action.compress();
    assert.equal(app.requests.length, 0);
    app.Action.decompress();
    app.Response.dispatch({ type: "Scanned" });
    app.Action.analyse();
    assert.deepEqual(app.requests.map(request => request.type), ["Decompress", "Analyse"]);
    app.Gui.scanning();
    finishScan(app);
    assert.equal(app.Gui.needs_analysis(), false);
    assert.equal(app.element("Button_Compress").visible, true);
});

test("thorough mode labels its savings as measured during compression, not a zero-byte estimate", () => {
    const app = application();
    app.Response.dispatch(config({ analysis_mode: "Thorough" }));
    finishScan(app);
    assert.equal(app.element("Estimated_Sampled").visible, false);
    assert.equal(app.element("Estimated_Thorough").visible, true);
    assert.match(app.$("#Analysis_Mode_Help").text(), /longer.*writes/);
});

test("editing analysis mode help does not apply an unsaved setting", () => {
    const app = application();
    app.Response.dispatch(config());
    app.$("#Analysis_Mode").val("Thorough").trigger("change");
    assert.match(app.$("#Analysis_Mode_Help").text(), /every eligible file/);
    finishScan(app);
    assert.equal(app.element("Estimated_Sampled").visible, true);
    assert.equal(app.element("Estimated_Thorough").visible, false);
});

test("allocation bars retain percent precision and clear when switching to an empty folder", () => {
    const app = application();
    finishScan(app);
    for (const id of ["Breakdown_Compressed", "Breakdown_Compressible", "Breakdown_Skipped"]) {
        assert.equal(app.element(id).style.width, "33.33%");
    }
    app.Gui.reset_folder_summary();
    for (const id of ["Breakdown_Compressed", "Breakdown_Compressible", "Breakdown_Skipped"]) {
        assert.equal(app.element(id).style.width, "0.00%");
    }
    assert.equal(app.element("File_Count_Breakdown").visible, false);
});

test("changing units redraws an existing folder summary immediately", () => {
    const app = application();
    const info = summary();
    info.logical_size = 1024;
    finishScan(app, info);
    assert.equal(app.$("#Size_Logical").text(), "1.00 KiB");
    app.Gui.set_decimal(true);
    assert.equal(app.$("#Size_Logical").text(), "1.02 KB");
});

test("folder and file viewers use the correct columns and treat paths as text", () => {
    const app = application();
    const maliciousPath = '<img src=x onerror="throw 1">';
    app.Response.dispatch(compressedView({ items: [{ kind: "Folder", count: 2, path: maliciousPath, logical_size: 8192, physical_size: 4096 }] }));
    let header = app.element("Compressed_View_Head").children[0];
    let row = app.element("Compressed_View_Body").children[0];
    assert.equal(header.children.length, 5);
    assert.ok(header.children.every(cell => cell.tag === "th" && cell.attributes.scope === "col"));
    assert.equal(row.children[4].text, maliciousPath);
    assert.equal(row.children[4].children.length, 0);
    app.$("#Compressed_View_Mode").val("files").trigger("change");
    assert.deepEqual(app.requests.pop(), { type: "ViewCompressed", view: "files", query: "", page: 0 });
    app.Response.dispatch(compressedView({ view: "files", items: [{ kind: "File", path: "data.bin", logical_size: 8192, physical_size: 4096 }] }));
    header = app.element("Compressed_View_Head").children[0];
    row = app.element("Compressed_View_Body").children[0];
    assert.deepEqual(header.children.map(cell => cell.text), ["Logical", "On-disk", "Saved", "File"]);
    assert.equal(row.children.length, 4);
    assert.equal(app.$("#Compressed_View_Title").text(), "Compressed Files");
});

test("stale compressed-view replies cannot replace the latest search or mode", () => {
    const app = application();
    app.Response.dispatch(compressedView());
    app.$("#Compressed_View_Search").val("updated").trigger("input");
    app.Response.dispatch(compressedView({ root: "stale" }));
    assert.equal(app.$("#Compressed_View_Root").text(), "C:\\Games");
    app.advance(200);
    assert.deepEqual(app.requests[0], { type: "ViewCompressed", view: "folders", query: "updated", page: 0 });
    app.Gui.set_compressed_mode("files");
    app.Response.dispatch(compressedView({ query: "updated", root: "stale folder mode" }));
    assert.equal(app.$("#Compressed_View_Root").text(), "C:\\Games");
});

test("empty file views use the correct colspan and pagination state", () => {
    const app = application();
    app.Gui.set_compressed_mode("files");
    app.Response.dispatch(compressedView({ view: "files", items: [], total: 0 }));
    const cell = app.element("Compressed_View_Body").children[0].children[0];
    assert.equal(cell.attributes.colspan, "4");
    assert.equal(cell.text, "No matching compressed files");
    assert.equal(app.element("Button_Compressed_Previous").disabled, true);
    assert.equal(app.element("Button_Compressed_Next").disabled, true);
});

test("operation failures remain visible after completion and are cleared for the next task", () => {
    const app = application();
    app.Gui.compacting();
    const error = "C:\\Games\\<script>.bin: Access denied";
    app.Response.dispatch({ type: "OperationSummary", operation: "Compression", changed: 7, not_beneficial: 2, skipped: 1, failed: 3, errors: [error] });
    app.Response.dispatch({ type: "Status", status: "Finished", pct: 1 });
    finishScan(app);
    assert.equal(app.element("Operation_Report").visible, true);
    assert.equal(app.$("#Operation_Failed").text(), "3");
    assert.equal(app.$("#Operation_Error_Heading").text(), "Errors (showing 1 of 3)");
    assert.equal(app.element("Operation_Errors").children[0].text, error);
    assert.equal(app.element("Operation_Errors").children[0].children.length, 0);
    app.Gui.compacting();
    assert.equal(app.element("Operation_Report").visible, false);
    assert.equal(app.element("Operation_Errors").children.length, 0);
});

test("progress stays in range and indeterminate status removes the value attribute", () => {
    const app = application();
    app.Gui.set_status("Finishing", 1.5);
    assert.equal(app.$("#Activity_Progress").val(), "1");
    app.Gui.set_status("Starting", -0.5);
    assert.equal(app.$("#Activity_Progress").val(), "0");
    app.$("#Activity_Progress").attr("value", "0");
    app.Gui.set_status("Analysing", null);
    assert.equal(app.$("#Activity_Progress").attr("value"), undefined);
});
