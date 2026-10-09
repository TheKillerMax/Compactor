/* jshint strict: true, esversion: 5, browser: true */

var Util = (function() {
	"use strict";

	var powers = '_KMGTPEZY';
	var monotime = function() { return Date.now(); };

	if (window.performance && window.performance.now)
		monotime = function() { return window.performance.now(); };

	return {
		debounce: function(callback, delay) {
			var timeout;
			var fn = function() {
				var context = this;
				var args = arguments;

				clearTimeout(timeout);
				timeout = setTimeout(function() {
					timeout = null;
					callback.apply(context, args);
				}, delay);
			};
			fn.clear = function() {
				clearTimeout(timeout);
				timeout = null;
			};

			return fn;
		},

		throttle: function(callback, delay) {
			var timeout;
			var last;
			var fn = function() {
				var context = this;
				var args = arguments;
				var now = monotime();

				if (last && now < last + delay) {
					clearTimeout(timeout);
					timeout = setTimeout(function() {
						timeout = null;
						last = now;
						callback.apply(context, args);
					}, delay);
				} else {
					last = now;
					callback.apply(context, args);
				}
			};
			fn.clear = function() {
				clearTimeout(timeout);
				timeout = null;
			};

			return fn;
		},

		format_number: function(number, digits) {
			if (digits === undefined) digits = 2;
			return number.toLocaleString("en", {minimumFractionDigits: digits, maximumFractionDigits: digits});
		},

		setting_number: function(value, minimum, maximum, integer) {
			var text = String(value).replace(/^\s+|\s+$/g, "");
			if (!/^[-+]?(?:\d+(?:\.\d*)?|\.\d+)(?:e[-+]?\d+)?$/i.test(text)) return null;
			var number = Number(text);
			if (!isFinite(number) || number < minimum || number > maximum) return null;
			if (integer && Math.floor(number) !== number) return null;
			return number;
		},

		bytes_to_human_dec: function(bytes) {
			for (var i = powers.length - 1; i > 0; i--) {
				var div = Math.pow(10, 3 * i);
				if (bytes >= div) {
					return Util.format_number(bytes / div, 2) + " " + powers[i] + 'B';
				}
			}

			return Util.format_number(bytes, 0) + ' B';
		},

		bytes_to_human_bin: function(bytes) {
			for (var i = powers.length - 1; i > 0; i--) {
				var div = Math.pow(2, 10*i);
				if (bytes >= div) {
					return Util.format_number(bytes / div, 2) + " " + powers[i] + 'iB';
				}
			}

			return Util.format_number(bytes, 0) + ' B';
		},

		human_to_bytes: function(human) {
			if (!human) return null;
			var num = parseFloat(human);

			var match = (/\s*([KMGTPEZY])(i)?([Bb])?\s*$/i).exec(human);
			if (match) {
				var pow = (match[2] == 'i') ? 1024 : 1000;
				num *= Math.pow(pow, powers.indexOf(match[1].toUpperCase()));
			}

			return num;
		},

		number_to_human: function(num) {
			for (var i = powers.length - 1; i > 0; i--) {
				var div = Math.pow(10, 3*i);
				if (num >= div) {
					return Util.format_number(num / div, 2) + powers[i];
				}
			}

			return num;
		},

		human_to_number: function(human) {
			if (!human) return null;
			var num = parseFloat(human);

			var match = (/\s*([KMGTPEZY])\s*$/i).exec(human);

			if (match) {
				num *= Math.pow(1000, powers.indexOf(match[1].toUpperCase()));
			}

			return num;
		},

		sformat: function() {
			var args = arguments;
			return args[0].replace(/\{(\d+)\}/g, function (m, n) { return args[parseInt(n, 10) + 1]; });
		},

		range: function(a, b, step) {
			if (!step) step = 1;
			var arr = [];
			for (var i = a; i < b; i += step) {
				arr.push(i);
			}

			return arr;
		}
	};
})();

Util.bytes_to_human = Util.bytes_to_human_bin;

var Action = (function() {
	"use strict";

	return {
		open_url: function(url) {
			external.invoke(JSON.stringify({ type: 'OpenUrl', url: url }));
		},

		reset_config: function() {
			if (Gui.is_busy()) return;
			external.invoke(JSON.stringify({ type: 'ResetConfig' }));
		},

		save_config: function(config) {
			if (Gui.is_busy()) return;
			config.type = 'SaveConfig';
			external.invoke(JSON.stringify(config));
		},

		choose_folder: function() {
			if (Gui.is_busy()) return;
			Gui.operation_requested();
			external.invoke(JSON.stringify({ type: 'ChooseFolder' }));
		},

		compress: function() {
			if (Gui.is_busy() || Gui.needs_analysis()) return;
			Gui.operation_requested();
			external.invoke(JSON.stringify({ type: 'Compress' }));
		},

		decompress: function() {
			if (Gui.is_busy()) return;
			Gui.operation_requested();
			external.invoke(JSON.stringify({ type: 'Decompress' }));
		},

		decompress_and_exclude: function() {
			if (Gui.is_busy()) return;
			Gui.operation_requested();
			external.invoke(JSON.stringify({ type: 'DecompressAndExclude' }));
		},

		view_compressed: function(view, query, page) {
			if (Gui.is_busy()) return;
			external.invoke(JSON.stringify({
				type: 'ViewCompressed',
				view: view,
				query: query,
				page: page
			}));
		},

		pause: function() {
			external.invoke(JSON.stringify({ type: 'Pause' }));
		},

		resume: function() {
			external.invoke(JSON.stringify({ type: 'Resume' }));
		},

		analyse: function() {
			if (Gui.is_busy()) return;
			Gui.operation_requested();
			external.invoke(JSON.stringify({ type: 'Analyse' }));
		},

		stop: function() {
			external.invoke(JSON.stringify({ type: 'Stop' }));
		},

		quit: function() {
			external.invoke(JSON.stringify({ type: 'Quit' }));
		}
	};
})();

var Response = (function() {
	"use strict";

	var pendingStatus = null;
	var pendingSummary = null;
	var flushTimer = null;

	var apply = function(msg) {
		switch(msg.type) {
			case "Config":
				Gui.set_decimal(msg.decimal);
				Gui.set_compression(msg.compression);
				Gui.set_analysis_mode(msg.analysis_mode || "Sampled");
				Gui.set_min_savings(msg.min_savings_percent);
				Gui.set_max_threads(msg.max_threads);
				Gui.set_compression_priority(msg.compression_priority);
				Gui.set_hdd_single_thread(msg.hdd_single_thread);
				Gui.set_protect_direct_storage(msg.protect_direct_storage);
				Gui.set_excludes(msg.excludes);
				break;

			case "Folder":
				Gui.set_folder(msg.path);
				break;

			case "Version":
				Gui.version(msg.date, msg.version);
				break;

			case "Status":
				Gui.set_status(msg.status, msg.pct);
				break;

			case "Error":
				Gui.operation_start_failed();
				window.alert(msg.title + "\n\n" + msg.message);
				break;

			case "Paused":
			case "Resumed":
			case "Stopped":
			case "Scanned":
			case "Compacting":
				Gui[msg.type.toLowerCase()]();
				break;

			case "FolderSummary":
				Gui.set_folder_summary(msg.info);
				break;

			case "CompressedView":
				Gui.set_compressed_view(msg);
				break;

			case "AnalysisInvalidated":
				Gui.analysis_invalidated();
				break;

			case "OperationSummary":
				Gui.set_operation_summary(msg);
				break;
		}
	};

	var flush = function() {
		if (flushTimer !== null) {
			clearTimeout(flushTimer);
			flushTimer = null;
		}

		var status = pendingStatus;
		var summary = pendingSummary;
		pendingStatus = null;
		pendingSummary = null;

		if (status) apply(status);
		if (summary) apply(summary);
	};

	return {
		dispatch: function(msg) {
			// High-frequency scan/compression updates are latest-wins. Coalescing
			// them avoids repeatedly rebuilding the same DOM within one paint
			// window while discrete state changes still arrive immediately.
			if (msg.type == "Status") {
				pendingStatus = msg;
			} else if (msg.type == "FolderSummary") {
				pendingSummary = msg;
			} else {
				flush();
				apply(msg);
				return;
			}

			if (flushTimer === null) {
				flushTimer = setTimeout(flush, 50);
			}
		}
	};
})();

var Gui = (function() {
	"use strict";

	var busy = false;
	var awaitingOperation = false;
	var analysisDirty = false;
	var analysisMode = "Sampled";
	var lastFolderSummary = null;
	var lastCompressedView = null;

	var compressedView = {
		view: "folders",
		page: 0,
		pages: 1
	};

	var compressedSearch = Util.debounce(function() {
		Gui.request_compressed(0);
	}, 200);

	var addTableCell = function(row, value, className, header) {
		var cell = $(header ? "<th></th>" : "<td></td>").text(value);
		if (header) cell.attr("scope", "col");
		if (className) cell.addClass(className);
		row.append(cell);
	};

	var setBusy = function(active) {
		busy = active;
		$("#Button_Folder, #Button_Save, #Button_Reset, #Button_Analyse, #Button_Compress, #Button_Decompress, #Button_Decompress_Exclude, #Button_View_Compressed, #Settings input, #Settings select, #Settings textarea")
			.prop("disabled", active);
		$("#Settings_Busy").toggle(active);
	};

	var updateEstimateDisplay = function() {
		$("#Estimated_Sampled").toggle(!analysisDirty && analysisMode === "Sampled");
		$("#Estimated_Thorough").toggle(!analysisDirty && analysisMode === "Thorough");
		$("#Estimated_Stale").toggle(analysisDirty);
		$("#Analysis_Invalidated").toggle(analysisDirty);
	};

	return {
		boot: function() {
			$("a[href]").on("click", function(e) {
				e.preventDefault();
				Action.open_url($(this).attr("href"));
				return false;
			});

			$("#Button_Save").on("click", Gui.save_settings);

			$("#Button_Reset").on("click", function() {
				Action.reset_config();
			});

			$("#Compressed_View_Search").on("input", compressedSearch);
			$("#Compressed_View_Mode").on("change", function() {
				Gui.set_compressed_mode($(this).val());
			});
			$("#Analysis_Mode").on("change", function() {
				Gui.update_analysis_help($(this).val());
			});
			Gui.update_analysis_help("Sampled");
		},

		is_busy: function() {
			return busy;
		},

		needs_analysis: function() {
			return analysisDirty;
		},

		operation_requested: function() {
			awaitingOperation = true;
			setBusy(true);
		},

		operation_start_failed: function() {
			if (awaitingOperation) Gui.scanned();
		},

		save_settings: function() {
			if (busy) return;
			var minSavings = Util.setting_number($("#Min_Savings").val(), 0, 100, false);
			if (minSavings === null) {
				window.alert("Minimum savings must be a number between 0 and 100%.");
				return;
			}
			var maxThreads = Util.setting_number($("#Max_Threads").val(), 0, 16, true);
			if (maxThreads === null) {
				window.alert("Maximum worker threads must be a whole number from 0 (Auto) to 16.");
				return;
			}

			Action.save_config({
				decimal: $("#SI_Units").val() == "D",
				compression: $("#Compression_Mode").val(),
				analysis_mode: $("#Analysis_Mode").val(),
				min_savings_percent: minSavings,
				max_threads: maxThreads,
				compression_priority: $("#Compression_Priority").val(),
				hdd_single_thread: $("#HDD_Single_Thread").prop("checked"),
				protect_direct_storage: $("#Protect_DirectStorage").prop("checked"),
				excludes: $("#Excludes").val()
			});
		},

		page: function(page) {
			$("nav button").removeClass("active");
			$("#Button_Page_" + page).addClass("active");
			$("section.page").hide();
			$("#" + page).show();
		},

		open_compressed_view: function() {
			if (busy) return;
			compressedSearch.clear();
			compressedView.page = 0;
			compressedView.pages = 1;
			$("#Compressed_View_Mode").val(compressedView.view);
			$("#Compressed_View_Search").val("");
			Gui.page("CompressedFiles");
			Gui.request_compressed(0);
		},

		set_compressed_mode: function(view) {
			if (busy) return;
			compressedSearch.clear();
			compressedView.view = view === "files" ? "files" : "folders";
			$("#Compressed_View_Mode").val(compressedView.view);
			Gui.request_compressed(0);
		},

		request_compressed: function(page) {
			var query = $("#Compressed_View_Search").val() || "";
			var requestedPage = parseInt(page, 10);
			if (isNaN(requestedPage) || requestedPage < 0) requestedPage = 0;
			Action.view_compressed(compressedView.view, query, requestedPage);
		},

		compressed_page: function(delta) {
			Gui.request_compressed(compressedView.page + delta);
		},

		set_compressed_view: function(data) {
			// A search/mode can change while an earlier request is in flight.
			// Keep that earlier response from replacing the user's latest view.
			var view = data.view === "files" ? "files" : "folders";
			var query = String($("#Compressed_View_Search").val() || "").replace(/^\s+|\s+$/g, "");
			if (view !== compressedView.view || data.query !== query) return;
			lastCompressedView = data;
			compressedView.page = data.page;
			compressedView.pages = data.pages;
			var folders = view === "folders";
			var itemName = folders ? "folders" : "files";
			var columns = folders ? 5 : 4;

			$("#Compressed_View_Title").text(folders ? "Compressed Folders" : "Compressed Files");
			$("#Compressed_View_Search").attr("placeholder", "Filter " + itemName + " by path");
			$("#Compressed_View_Root").text(data.root);
			$("#Compressed_View_Summary").text(
				Util.format_number(data.compressed_count, 0) + " compressed files - " +
				Util.bytes_to_human(data.logical_size) + " logical - " +
				Util.bytes_to_human(data.physical_size) + " on-disk - " +
				Util.bytes_to_human(Math.max(0, data.logical_size - data.physical_size)) + " saved"
			);

			var head = $("#Compressed_View_Head").empty();
			var body = $("#Compressed_View_Body").empty();
			var header = $("<tr></tr>");

			if (folders) addTableCell(header, "Files", null, true);
			addTableCell(header, "Logical", null, true);
			addTableCell(header, "On-disk", null, true);
			addTableCell(header, "Saved", null, true);
			addTableCell(header, folders ? "Folder" : "File", "path", true);
			head.append(header);

			data.items.forEach(function(item) {
				var row = $("<tr></tr>");
				if (folders) addTableCell(row, Util.format_number(item.count, 0));
				addTableCell(row, Util.bytes_to_human(item.logical_size));
				addTableCell(row, Util.bytes_to_human(item.physical_size));
				addTableCell(row, Util.bytes_to_human(Math.max(0, item.logical_size - item.physical_size)));
				addTableCell(row, item.path, "path");
				body.append(row);
			});

			if (data.items.length === 0) {
				var empty = $("<tr></tr>");
				empty.append(
					$("<td></td>")
						.attr("colspan", columns)
						.addClass("empty")
						.text("No matching compressed " + itemName)
				);
				body.append(empty);
			}

			$("#Compressed_View_Page").text(
				"Page " + Util.format_number(data.page + 1, 0) + " of " +
				Util.format_number(data.pages, 0) + " - " +
				Util.format_number(data.total, 0) + " " + itemName
			);
			$("#Button_Compressed_Previous").prop("disabled", data.page <= 0);
			$("#Button_Compressed_Next").prop("disabled", data.page + 1 >= data.pages);
		},

		version: function(date, version) {
			$(".compile-date").text(date);
			$(".version").text(version);
		},

		set_decimal: function(dec) {
			var field = $("#SI_Units");
			if (dec) {
				field.val("D");
				Util.bytes_to_human = Util.bytes_to_human_dec;
			} else {
				field.val("I");
				Util.bytes_to_human = Util.bytes_to_human_bin;
			}
			if (lastFolderSummary) Gui.set_folder_summary(lastFolderSummary);
			if (lastCompressedView) Gui.set_compressed_view(lastCompressedView);
		},

		set_compression: function(compression) {
			$("#Compression_Mode").val(String(compression).toUpperCase());
		},

		set_analysis_mode: function(mode) {
			analysisMode = mode === "Thorough" ? "Thorough" : "Sampled";
			$("#Analysis_Mode").val(analysisMode);
			Gui.update_analysis_help(analysisMode);
			updateEstimateDisplay();
		},

		update_analysis_help: function(mode) {
			$("#Analysis_Mode_Help").text(mode === "Thorough"
				? "Tries every eligible file without the sampling filter. This can find extra savings, but takes longer and may perform more writes."
				: "Uses a quick sample to skip files unlikely to benefit. Some files that would compress well may be missed.");
		},

		set_min_savings: function(percent) {
			$("#Min_Savings").val(percent);
		},

		set_max_threads: function(threads) {
			$("#Max_Threads").val(threads);
		},

		set_compression_priority: function(priority) {
			$("#Compression_Priority").val(priority);
		},

		set_hdd_single_thread: function(enabled) {
			$("#HDD_Single_Thread").prop("checked", enabled);
		},

		set_protect_direct_storage: function(enabled) {
			$("#Protect_DirectStorage").prop("checked", enabled);
		},

		set_excludes: function(excludes) {
			$("#Excludes").val(excludes);
		},

		set_folder: function(folder) {
			var bits = folder.split(/:\\|\\/).map(function(x) { return document.createTextNode(x); });
			var end = bits.pop();

			var button = $("#Button_Folder");
			button.empty();
			bits.forEach(function(bit) {
				button.append(bit);
				button.append($("<span>&gt;</span>"));
			});
			button.append(end);

			Gui.scanning();
		},

		set_status: function(status, pct) {
			$("#Activity_Text").text(status).attr("title", status);
			if (typeof pct === "number" && isFinite(pct)) {
				$("#Activity_Progress").val(Math.max(0, Math.min(1, pct)));
			} else {
				$("#Activity_Progress").removeAttr("value");
			}
		},

		scanning: function() {
			awaitingOperation = false;
			setBusy(true);
			analysisDirty = false;
			lastCompressedView = null;
			compressedSearch.clear();
			Gui.clear_operation_summary();
			Gui.reset_folder_summary();
			$("#Activity").show();
			$("#Analysis").show();
			$("#Button_Pause").show();
			$("#Button_Resume").hide();
			$("#Button_Stop").show();
			$("#Button_Analyse").hide();
			$("#Button_Compress").hide();
			$("#Button_Decompress").hide();
			$("#Button_Decompress_Exclude").hide();
			$("#Button_View_Compressed").hide();
			$("#Command").show();
		},

		compacting: function() {
			awaitingOperation = false;
			setBusy(true);
			Gui.clear_operation_summary();
			$("#Activity").show();
			$("#Button_Pause").show();
			$("#Button_Resume").hide();
			$("#Button_Stop").show();
			$("#Button_Analyse").hide();
			$("#Button_Compress").hide();
			$("#Button_Decompress").hide();
			$("#Button_Decompress_Exclude").hide();
			$("#Button_View_Compressed").hide();
		},

		paused: function() {
			$("#Button_Pause").hide();
			$("#Button_Resume").show();
		},

		resumed: function() {
			$("#Button_Pause").show();
			$("#Button_Resume").hide();
		},

		stopped: function() {
			Gui.scanned();
		},

		scanned: function() {
			awaitingOperation = false;
			setBusy(false);
			$("#Button_Pause").hide();
			$("#Button_Resume").hide();
			$("#Button_Stop").hide();
			$("#Button_Analyse").show();

			if (!analysisDirty && lastFolderSummary && lastFolderSummary.compressible.count > 0) {
				$("#Button_Compress").show();
			} else {
				$("#Button_Compress").hide();
			}

			if (lastFolderSummary && lastFolderSummary.compressed.count > 0) {
				$("#Button_Decompress").show();
				$("#Button_Decompress_Exclude").show();
				$("#Button_View_Compressed").show();
			} else {
				$("#Button_Decompress").hide();
				$("#Button_Decompress_Exclude").hide();
				$("#Button_View_Compressed").hide();
			}
		},

		analysis_invalidated: function() {
			analysisDirty = true;
			$("#Button_Compress").hide();
			updateEstimateDisplay();
		},

		clear_operation_summary: function() {
			$("#Operation_Report").hide();
			$("#Operation_Errors").empty();
		},

		set_operation_summary: function(data) {
			$("#Operation_Title").text(data.operation + " results");
			$("#Operation_Changed").text(Util.format_number(data.changed, 0));
			$("#Operation_Not_Beneficial").text(Util.format_number(data.not_beneficial, 0));
			$("#Operation_Skipped").text(Util.format_number(data.skipped, 0));
			$("#Operation_Failed").text(Util.format_number(data.failed, 0));
			var errors = data.errors || [];
			var list = $("#Operation_Errors").empty();
			errors.forEach(function(error) {
				list.append($("<li></li>").text(error));
			});
			$("#Operation_Error_Heading").text(data.failed > errors.length
				? "Errors (showing " + Util.format_number(errors.length, 0) + " of " + Util.format_number(data.failed, 0) + ")"
				: "Errors");
			$("#Operation_Error_Details").toggle(errors.length > 0);
			$("#Operation_Report").show();
		},

		reset_folder_summary: function() {
			Gui.set_folder_summary({
				logical_size: 0,
				physical_size: 0,
				direct_storage: false,
				compressed: {count: 0, logical_size: 0, physical_size: 0, estimated_physical_size: 0},
				compressible: {count: 0, logical_size: 0, physical_size: 0, estimated_physical_size: 0},
				skipped: {count: 0, logical_size: 0, physical_size: 0, estimated_physical_size: 0}
			});
		},

		set_folder_summary: function(data) {
			lastFolderSummary = data;
			$("#Size_Logical").text(Util.bytes_to_human(data.logical_size));
			$("#Size_Physical").text(Util.bytes_to_human(data.physical_size));

			if (data.logical_size > 0) {
				var ratio = (data.physical_size / data.logical_size);
				$("#Compress_Ratio").text(Util.format_number(ratio, 2));
			} else {
				$("#Compress_Ratio").text("1.00");
			}

			$("#Compressed_Size").text(Util.bytes_to_human(data.compressed.physical_size));
			$("#Compressible_Size").text(Util.bytes_to_human(data.compressible.physical_size));
			$("#Skipped_Size").text(Util.bytes_to_human(data.skipped.physical_size));

			var total = data.physical_size;
			var width = function(size) {
				return (total > 0 ? Math.max(0, Math.min(100, 100 * size / total)) : 0).toFixed(2) + "%";
			};
			document.getElementById("Breakdown_Compressed").style.width = width(data.compressed.physical_size);
			document.getElementById("Breakdown_Compressible").style.width = width(data.compressible.physical_size);
			document.getElementById("Breakdown_Skipped").style.width = width(data.skipped.physical_size);
			$("#File_Count_Breakdown").toggle(total > 0);

			var estimatedCandidateSize = data.compressible.estimated_physical_size;
			if (estimatedCandidateSize === undefined || estimatedCandidateSize === null) {
				estimatedCandidateSize = data.compressible.physical_size;
			}
			var estimatedSavings = Math.max(0, data.compressible.physical_size - estimatedCandidateSize);
			var estimatedTotal = Math.max(0, data.physical_size - estimatedSavings);

			$("#Space_Saved").text(Util.bytes_to_human(Math.max(0, data.compressed.logical_size - data.compressed.physical_size)));
			$("#Estimated_Savings").text(Util.bytes_to_human(estimatedSavings));
			$("#Estimated_Physical").text(Util.bytes_to_human(estimatedTotal));

			$("#File_Count_Compressed").text(Util.format_number(data.compressed.count, 0));
			$("#File_Count_Compressible").text(Util.format_number(data.compressible.count, 0));
			$("#File_Count_Skipped").text(Util.format_number(data.skipped.count, 0));
			$("#DirectStorage_Warning").toggle(!!data.direct_storage);
			updateEstimateDisplay();
		},

		analysis_complete: function() {
			$("#Activity").hide();
			$("#Analysis").show();
		}
	};
})();

$(document).ready(Gui.boot);
