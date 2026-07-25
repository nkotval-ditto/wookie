"use strict";

(() => {
  const POLL_INTERVAL_MS = 1500;
  const MOVE_DURATION_MS = 210;
  const STATUSES = ["todo", "doing", "blocked", "done"];
  const STATUS_LABELS = {
    todo: "To do",
    doing: "Doing",
    blocked: "Blocked",
    done: "Done",
  };

  const elements = {
    sessionTitle: document.getElementById("session-title"),
    sessionKicker: document.getElementById("session-kicker"),
    sessionAgent: document.getElementById("session-agent"),
    sessionId: document.getElementById("session-id"),
    sessionProgress: document.getElementById("session-progress"),
    linearFact: document.getElementById("linear-fact"),
    linearProject: document.getElementById("linear-project"),
    connectionStatus: document.getElementById("connection-status"),
    connectionLabel: document.getElementById("connection-label"),
    stateBanner: document.getElementById("state-banner"),
    stateBannerTitle: document.getElementById("state-banner-title"),
    stateBannerDetail: document.getElementById("state-banner-detail"),
    planHeading: document.getElementById("plan-heading"),
    planDescription: document.getElementById("plan-description"),
    planCount: document.getElementById("plan-count"),
    boardEmpty: document.getElementById("board-empty"),
    boardColumns: document.getElementById("board-columns"),
    timelineList: document.getElementById("timeline-list"),
    timelineCount: document.getElementById("timeline-count"),
    timelineEmpty: document.getElementById("timeline-empty"),
    timelineOmitted: document.getElementById("timeline-omitted"),
    notificationScanWarning: document.getElementById(
      "notification-scan-warning",
    ),
    notificationScanTitle: document.getElementById("notification-scan-title"),
    notificationScanDetail: document.getElementById("notification-scan-detail"),
    notificationWarningList: document.getElementById(
      "notification-warning-list",
    ),
    lastUpdated: document.getElementById("last-updated"),
    dialog: document.getElementById("segment-dialog"),
    drawerClose: document.getElementById("drawer-close"),
    drawerStatus: document.getElementById("drawer-status"),
    drawerTitle: document.getElementById("drawer-title"),
    drawerJustification: document.getElementById("drawer-justification"),
    drawerReadiness: document.getElementById("drawer-readiness"),
    drawerDependencies: document.getElementById("drawer-dependencies"),
    drawerDecisions: document.getElementById("drawer-decisions"),
    drawerVerification: document.getElementById("drawer-verification"),
    drawerGuideId: document.getElementById("drawer-guide-id"),
    guideReload: document.getElementById("guide-reload"),
    guideState: document.getElementById("guide-state"),
    guidePage: document.getElementById("guide-page"),
    guideTitle: document.getElementById("guide-title"),
    guideDescription: document.getElementById("guide-description"),
    guideTags: document.getElementById("guide-tags"),
    guideBody: document.getElementById("guide-body"),
    screenReaderUpdates: document.getElementById("screen-reader-updates"),
    lists: Object.fromEntries(
      STATUSES.map((status) => [
        status,
        document.getElementById(`${status}-list`),
      ]),
    ),
    counts: Object.fromEntries(
      STATUSES.map((status) => [
        status,
        document.getElementById(`${status}-count`),
      ]),
    ),
  };

  const state = {
    etag: "",
    snapshot: null,
    timer: null,
    requestInFlight: false,
    failures: 0,
    selectedSegmentId: null,
    guideRequest: null,
    initialized: false,
    terminal: false,
  };

  function asObject(value) {
    return value && typeof value === "object" && !Array.isArray(value)
      ? value
      : {};
  }

  function asArray(value) {
    if (Array.isArray(value)) {
      return value;
    }
    if (value && typeof value === "object") {
      return Object.values(value);
    }
    return [];
  }

  function text(value, fallback = "") {
    if (typeof value === "string") {
      return value.trim();
    }
    if (typeof value === "number" || typeof value === "boolean") {
      return String(value);
    }
    return fallback;
  }

  function integer(value, fallback = 0) {
    const parsed = Number.parseInt(value, 10);
    return Number.isFinite(parsed) && parsed >= 0 ? parsed : fallback;
  }

  function bool(value, fallback = false) {
    return typeof value === "boolean" ? value : fallback;
  }

  function stringList(value) {
    if (typeof value === "string") {
      const candidate = value.trim();
      return candidate ? [candidate] : [];
    }
    return asArray(value)
      .map((item) => {
        if (typeof item === "string") {
          return item.trim();
        }
        const object = asObject(item);
        return text(
          object.summary ??
            object.title ??
            object.text ??
            object.id ??
            object.path,
        );
      })
      .filter(Boolean);
  }

  function segmentId(value, index) {
    return text(value, `segment-${index + 1}`);
  }

  function normalizeStatus(value) {
    const candidate = text(value, "todo").toLowerCase().replace(/[_\s-]+/g, "");
    const aliases = {
      todo: "todo",
      planned: "todo",
      pending: "todo",
      ready: "todo",
      doing: "doing",
      inprogress: "doing",
      active: "doing",
      blocked: "blocked",
      waiting: "blocked",
      done: "done",
      complete: "done",
      completed: "done",
    };
    return aliases[candidate] || "todo";
  }

  function normalizeSegment(raw, index) {
    const source = asObject(raw);
    const id = segmentId(source.id, index);
    const dependsOn = stringList(source.depends_on ?? source.dependencies);
    const blockedBy = stringList(source.blocked_by);
    const status = normalizeStatus(source.status);
    const readyFallback =
      status === "doing" || status === "done" || (dependsOn.length === 0 && blockedBy.length === 0);

    return {
      id,
      title: text(source.title, id),
      status,
      guide: text(source.guide ?? source.guide_id ?? source.page, "No guide attached"),
      justification: text(
        source.justification ?? source.reason,
        "No justification recorded.",
      ),
      decisions: stringList(source.decisions ?? source.architectural_decisions),
      verification: text(
        source.verification ?? source.acceptance ?? source.done_when,
        "No verification criteria recorded.",
      ),
      dependsOn,
      blockedBy,
      ready: bool(source.ready, readyFallback),
      order: Number.isFinite(Number(source.order)) ? Number(source.order) : index,
      linearIssue: null,
    };
  }

  function linearUrl(value) {
    const candidate = text(value);
    return candidate.startsWith("https://linear.app/") &&
      !candidate.includes("?") &&
      !candidate.includes("#")
      ? candidate
      : "";
  }

  function normalizeSnapshot(raw) {
    const source = asObject(raw);
    const session = asObject(source.session);
    const plan = asObject(source.plan);
    const title = text(
      source.title ?? plan.title ?? session.title ?? session.name,
      "Untitled plan",
    );
    const segments = asArray(source.segments ?? plan.segments).map(normalizeSegment);
    const linearSource = asObject(source.linear);
    const linearIssues = new Map(
      asArray(linearSource.issues).map((issue) => {
        const item = asObject(issue);
        return [
          text(item.segment_id),
          {
            id: text(item.id),
            url: linearUrl(item.url),
            status: normalizeStatus(item.status),
          },
        ];
      }),
    );
    segments.forEach((segment) => {
      segment.linearIssue = linearIssues.get(segment.id) || null;
    });
    const linearProject = asObject(linearSource.project);

    return {
      schema: text(source.schema),
      planSchema: text(source.plan_schema ?? plan.schema),
      planHash: text(source.plan_hash ?? plan.hash),
      title,
      description: text(
        source.description ?? plan.description ?? plan.summary,
        "Each segment is backed by its implementation guide.",
      ),
      session,
      segments,
      linear: {
        project: {
          id: text(linearProject.id),
          url: linearUrl(linearProject.url),
        },
        linkHash: text(linearSource.link_sha256),
        syncHash: text(linearSource.sync_sha256),
      },
      events: asArray(source.events),
      eventsTotal: integer(source.events_total, asArray(source.events).length),
      eventsOmitted: integer(source.events_omitted),
      notifications: asArray(source.notifications),
      notificationsTotal: integer(
        source.notifications_total ?? source.total,
        asArray(source.notifications).length,
      ),
      notificationsOmitted: integer(
        source.notifications_omitted ?? source.omitted,
      ),
      notificationsScanComplete:
        typeof source.notifications_scan_complete === "boolean"
          ? source.notifications_scan_complete
          : null,
      notificationWarningsTotal: integer(source.notification_warnings_total),
      notificationWarnings: asArray(source.notification_warnings)
        .map((warning) => {
          if (typeof warning === "string") {
            return warning.trim();
          }
          const item = asObject(warning);
          const message = text(
            item.message ?? item.summary ?? item.warning ?? item.reason,
          );
          const path = text(item.path);
          if (message && path) {
            return `${message} · ${path}`;
          }
          return message || path;
        })
        .filter(Boolean),
    };
  }

  function createElement(tagName, className, content) {
    const element = document.createElement(tagName);
    if (className) {
      element.className = className;
    }
    if (content !== undefined && content !== null) {
      element.textContent = String(content);
    }
    return element;
  }

  function replaceList(list, values, emptyMessage) {
    list.replaceChildren();
    const items = values.length > 0 ? values : [emptyMessage];
    items.forEach((value) => {
      list.append(createElement("li", "", value));
    });
  }

  function formatCount(value, singular, plural) {
    return `${value} ${value === 1 ? singular : plural}`;
  }

  function sessionValue(session, names, fallback = "—") {
    for (const name of names) {
      const value = text(session[name]);
      if (value) {
        return value;
      }
    }
    return fallback;
  }

  function captureCardPositions() {
    const positions = new Map();
    document.querySelectorAll(".plan-card[data-segment-id]").forEach((card) => {
      positions.set(card.dataset.segmentId, card.getBoundingClientRect());
    });
    return positions;
  }

  function animateCardMoves(previousPositions) {
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      return;
    }

    document.querySelectorAll(".plan-card[data-segment-id]").forEach((card) => {
      const previous = previousPositions.get(card.dataset.segmentId);
      if (!previous) {
        return;
      }

      const current = card.getBoundingClientRect();
      const deltaX = previous.left - current.left;
      const deltaY = previous.top - current.top;
      if (Math.abs(deltaX) < 1 && Math.abs(deltaY) < 1) {
        return;
      }

      if (typeof card.animate !== "function") {
        return;
      }

      card.animate(
        [
          {
            transform: `translate(${deltaX}px, ${deltaY}px)`,
            boxShadow: "0 12px 30px rgb(23 35 38 / 0.14)",
          },
          { transform: "translate(0, 0)", boxShadow: "none" },
        ],
        {
          duration: MOVE_DURATION_MS,
          easing: "cubic-bezier(0.2, 0.8, 0.2, 1)",
        },
      );
    });
  }

  function readinessFor(segment) {
    if (segment.status === "blocked" || segment.blockedBy.length > 0) {
      return {
        label:
          segment.blockedBy.length > 0
            ? `Blocked by ${segment.blockedBy.join(", ")}`
            : "Blocked",
        className: "is-blocked",
      };
    }
    if (!segment.ready) {
      return {
        label:
          segment.dependsOn.length > 0
            ? `Waiting on ${segment.dependsOn.join(", ")}`
            : "Not ready",
        className: "is-waiting",
      };
    }
    if (segment.status === "done") {
      return { label: "Verified complete", className: "" };
    }
    return { label: "Ready", className: "" };
  }

  function buildCard(segment, index) {
    const card = createElement("article", "plan-card");
    card.dataset.segmentId = segment.id;
    card.dataset.status = segment.status;
    card.setAttribute("role", "listitem");

    const openButton = createElement("button", "card-open-button");
    openButton.type = "button";
    openButton.setAttribute("aria-haspopup", "dialog");
    openButton.setAttribute(
      "aria-label",
      `Open details for ${segment.title}, ${STATUS_LABELS[segment.status]}`,
    );

    const indexLabel = createElement(
      "span",
      "card-index",
      `${String(index + 1).padStart(2, "0")} · ${segment.id}`,
    );
    const title = createElement("span", "card-title", segment.title);
    const justification = createElement(
      "span",
      "card-justification",
      segment.justification,
    );
    const meta = createElement("span", "card-meta");
    const readiness = readinessFor(segment);
    const readinessElement = createElement(
      "span",
      `readiness-pill ${readiness.className}`.trim(),
      readiness.label,
    );
    const guideButton = createElement("button", "guide-button", "Read guide");
    guideButton.type = "button";
    guideButton.setAttribute("aria-label", `Read the guide for ${segment.title}`);
    guideButton.addEventListener("click", (event) => {
      openSegment(segment, true);
    });
    meta.append(readinessElement);

    openButton.append(indexLabel, title, justification, meta);

    if (segment.dependsOn.length > 0) {
      const dependencyNote = createElement("span", "dependency-note");
      dependencyNote.append(
        createElement("strong", "", "Depends on"),
        document.createTextNode(segment.dependsOn.join(", ")),
      );
      openButton.append(dependencyNote);
    }

    openButton.addEventListener("click", () => openSegment(segment, false));
    card.append(openButton, guideButton);
    if (segment.linearIssue?.url) {
      const linearLink = createElement(
        "a",
        "card-linear",
        segment.linearIssue.id || "Linear",
      );
      linearLink.href = segment.linearIssue.url;
      linearLink.target = "_blank";
      linearLink.rel = "noopener noreferrer";
      linearLink.setAttribute(
        "aria-label",
        `Open Linear issue for ${segment.title}`,
      );
      card.append(linearLink);
    }
    return card;
  }

  function renderHeader(snapshot) {
    const session = snapshot.session;
    const done = snapshot.segments.filter(
      (segment) => segment.status === "done",
    ).length;
    const total = snapshot.segments.length;

    document.title = `${snapshot.title} · Wookie Plan`;
    elements.sessionTitle.textContent = snapshot.title;
    elements.sessionKicker.textContent = "Active session";
    elements.sessionAgent.textContent = sessionValue(session, [
      "agent",
      "agent_name",
      "owner",
      "model",
    ]);
    elements.sessionId.textContent = sessionValue(session, [
      "id",
      "session_id",
      "name",
    ]);
    elements.sessionProgress.textContent =
      total > 0 ? `${done}/${total} done` : "No segments";
    const projectUrl = snapshot.linear.project.url;
    elements.linearFact.hidden = !projectUrl;
    if (projectUrl) {
      elements.linearProject.href = projectUrl;
      elements.linearProject.textContent =
        snapshot.linear.project.id || "Open epic";
    } else {
      elements.linearProject.removeAttribute("href");
    }
    elements.planHeading.textContent = snapshot.title;
    elements.planDescription.textContent = snapshot.description;
    elements.planCount.textContent = formatCount(total, "segment", "segments");
  }

  function renderBoard(snapshot) {
    const previousPositions = captureCardPositions();
    const byStatus = Object.fromEntries(STATUSES.map((status) => [status, []]));

    snapshot.segments
      .slice()
      .sort((a, b) => a.order - b.order || a.id.localeCompare(b.id))
      .forEach((segment) => {
        byStatus[segment.status].push(segment);
      });

    STATUSES.forEach((status) => {
      const list = elements.lists[status];
      list.replaceChildren();
      byStatus[status].forEach((segment) => {
        const originalIndex = snapshot.segments.findIndex(
          (candidate) => candidate.id === segment.id,
        );
        list.append(buildCard(segment, originalIndex));
      });
      const count = byStatus[status].length;
      elements.counts[status].textContent = String(count);
      elements.counts[status].setAttribute(
        "aria-label",
        formatCount(count, "item", "items"),
      );
    });

    const isEmpty = snapshot.segments.length === 0;
    elements.boardEmpty.hidden = !isEmpty;
    elements.boardColumns.hidden = isEmpty;

    requestAnimationFrame(() => animateCardMoves(previousPositions));
  }

  function timestampOf(source) {
    const value = text(
      source.timestamp ??
        source.created_at ??
        source.time ??
        source.at ??
        source.updated_at,
    );
    const milliseconds = Date.parse(value);
    return {
      raw: value,
      milliseconds: Number.isNaN(milliseconds) ? 0 : milliseconds,
    };
  }

  function formatTimestamp(value) {
    if (!value) {
      return "Time unknown";
    }
    const parsed = new Date(value);
    if (Number.isNaN(parsed.getTime())) {
      return value;
    }
    return new Intl.DateTimeFormat(undefined, {
      hour: "numeric",
      minute: "2-digit",
      second: "2-digit",
    }).format(parsed);
  }

  function eventKind(source) {
    const candidate = text(
      source.log_kind ??
        source.kind ??
        source.type ??
        source.event ??
        source.action,
    )
      .toLowerCase()
      .replace(/[_\s-]+/g, "");
    if (
      candidate.includes("status") ||
      candidate.includes("transition") ||
      candidate.includes("moved")
    ) {
      return "transition";
    }
    if (candidate.includes("decision")) {
      return "decision";
    }
    if (candidate.includes("block")) {
      return "blocker";
    }
    if (candidate.includes("notification") || candidate.includes("notify")) {
      return "notification";
    }
    return "note";
  }

  function normalizedTimelineEvent(raw, index) {
    const source = asObject(raw);
    const payload = asObject(source.payload ?? source.data);
    const plan = asObject(source.plan);
    const combined = { ...payload, ...plan, ...source };
    if (plan.kind) {
      combined.kind = plan.kind;
    }
    if (plan.log_kind) {
      combined.log_kind = plan.log_kind;
    }
    const kind = eventKind(combined);
    const planKind = text(combined.kind).toLowerCase();
    const from = text(combined.from ?? combined.from_status);
    const to = text(combined.to ?? combined.to_status);
    const segment = text(
      combined.segment_title ?? combined.segment_id ?? combined.segment,
    );
    let title = text(combined.title);

    if (!title && kind === "transition") {
      title = segment ? `${segment} moved` : "Segment moved";
    } else if (!title && kind === "decision") {
      title = segment ? `Decision · ${segment}` : "Decision recorded";
    } else if (!title && kind === "blocker") {
      title = segment ? `Blocked · ${segment}` : "Blocker recorded";
    } else if (!title && planKind === "attached") {
      title = "Plan attached";
    } else if (!title && planKind === "archived") {
      title = "Session archived";
    } else if (!title) {
      title = segment ? `Update · ${segment}` : "Session update";
    }

    let summary = text(
      combined.summary ??
        combined.note ??
        combined.message ??
        combined.decision ??
        combined.reason ??
        combined.text,
    );
    if (!summary && from && to) {
      summary = `${STATUS_LABELS[normalizeStatus(from)] ?? from} → ${
        STATUS_LABELS[normalizeStatus(to)] ?? to
      }`;
    }

    const timestamp = timestampOf(combined);
    return {
      key: `event-${text(combined.id, index)}-${timestamp.raw}`,
      kind,
      title,
      summary,
      paths: stringList(combined.paths ?? combined.files ?? combined.path),
      timestamp,
      order: index,
    };
  }

  function normalizedNotification(raw, index) {
    const source = asObject(raw);
    const payload = asObject(source.payload ?? source.data);
    const combined = { ...payload, ...source };
    const timestamp = timestampOf(combined);
    const notificationKind = text(combined.kind)
      .replace(/[_-]+/g, " ")
      .replace(/^\w/, (character) => character.toUpperCase());
    const paths = stringList(
      combined.paths ??
        combined.files ??
        combined.path ??
        combined.page_id ??
        combined.page,
    );
    const affectedPathCount = integer(combined.affected_path_count);
    if (paths.length === 0 && affectedPathCount > 0) {
      paths.push(
        formatCount(affectedPathCount, "affected path", "affected paths"),
      );
    }

    return {
      key: `notification-${text(combined.id, index)}-${timestamp.raw}`,
      kind: "notification",
      title: text(
        combined.title ?? combined.subject ?? combined.page_title,
        notificationKind
          ? `${notificationKind} notification`
          : "Wookie notification",
      ),
      summary: text(
        combined.summary ??
          combined.description ??
          combined.message ??
          combined.reason,
        "A linked knowledge page changed.",
      ),
      paths,
      timestamp,
      order: index,
    };
  }

  function renderTimeline(snapshot) {
    const records = [
      ...snapshot.events.map(normalizedTimelineEvent),
      ...snapshot.notifications.map(normalizedNotification),
    ].sort(
      (a, b) =>
        b.timestamp.milliseconds - a.timestamp.milliseconds ||
        b.order - a.order,
    );

    elements.timelineList.replaceChildren();
    records.forEach((record) => {
      const item = createElement("li", "timeline-item");
      item.dataset.kind = record.kind;
      item.dataset.recordKey = record.key;

      const title = createElement("p", "timeline-title", record.title);
      const timeElement = createElement(
        "time",
        "timeline-time",
        formatTimestamp(record.timestamp.raw),
      );
      if (record.timestamp.raw) {
        timeElement.dateTime = record.timestamp.raw;
      }
      item.append(title, timeElement);

      if (record.summary) {
        item.append(createElement("p", "timeline-summary", record.summary));
      }
      if (record.paths.length > 0) {
        const paths = createElement("ul", "timeline-paths");
        record.paths.forEach((path) => {
          paths.append(createElement("li", "", path));
        });
        item.append(paths);
      }
      elements.timelineList.append(item);
    });

    elements.timelineEmpty.hidden = records.length > 0;
    const recordedTotal =
      snapshot.eventsTotal + snapshot.notificationsTotal;
    elements.timelineCount.textContent = formatCount(
      recordedTotal,
      "record",
      "records",
    );
    const omitted = snapshot.eventsOmitted + snapshot.notificationsOmitted;
    elements.timelineOmitted.hidden = omitted === 0;
    elements.timelineOmitted.textContent =
      omitted > 0
        ? `${formatCount(omitted, "older record", "older records")} omitted from this snapshot.`
        : "";

    const scanIncomplete = snapshot.notificationsScanComplete === false;
    const warningTotal = Math.max(
      snapshot.notificationWarningsTotal,
      snapshot.notificationWarnings.length,
    );
    const showScanWarning = scanIncomplete || warningTotal > 0;
    elements.notificationScanWarning.hidden = !showScanWarning;
    elements.notificationWarningList.replaceChildren();
    if (showScanWarning) {
      elements.notificationScanTitle.textContent = scanIncomplete
        ? "Notification scan incomplete"
        : "Notification scan warnings";
      const warningSummary =
        warningTotal > 0
          ? `${formatCount(warningTotal, "warning", "warnings")} reported.`
          : "Some notification records could not be scanned.";
      elements.notificationScanDetail.textContent = scanIncomplete
        ? `This timeline may omit notifications. ${warningSummary}`
        : warningSummary;
      snapshot.notificationWarnings.forEach((warning) => {
        elements.notificationWarningList.append(
          createElement("li", "", warning),
        );
      });
      elements.notificationWarningList.hidden =
        snapshot.notificationWarnings.length === 0;
    } else {
      elements.notificationScanTitle.textContent = "";
      elements.notificationScanDetail.textContent = "";
      elements.notificationWarningList.hidden = true;
    }
  }

  function setConnection(mode, label) {
    elements.connectionStatus.classList.remove(
      "is-connecting",
      "is-live",
      "is-offline",
      "is-closed",
    );
    elements.connectionStatus.classList.add(`is-${mode}`);
    elements.connectionLabel.textContent = label;
  }

  function showError(message, detail) {
    elements.stateBanner.hidden = false;
    elements.stateBannerTitle.textContent = message;
    elements.stateBannerDetail.textContent = detail;
  }

  function hideError() {
    elements.stateBanner.hidden = true;
  }

  function updateSelectedSegment(snapshot) {
    if (!state.selectedSegmentId || !elements.dialog.open) {
      return;
    }
    const segment = snapshot.segments.find(
      (candidate) => candidate.id === state.selectedSegmentId,
    );
    if (!segment) {
      elements.dialog.close();
      state.selectedSegmentId = null;
      elements.screenReaderUpdates.textContent =
        "The open segment is no longer part of this plan.";
      return;
    }
    renderDrawerSegment(segment);
  }

  function renderSnapshot(raw) {
    const snapshot = normalizeSnapshot(raw);
    const previous = state.snapshot;
    state.snapshot = snapshot;

    renderHeader(snapshot);
    renderBoard(snapshot);
    renderTimeline(snapshot);
    updateSelectedSegment(snapshot);

    const now = new Date();
    elements.lastUpdated.textContent = `Snapshot received ${new Intl.DateTimeFormat(
      undefined,
      {
        hour: "numeric",
        minute: "2-digit",
        second: "2-digit",
      },
    ).format(now)}`;

    if (previous && state.initialized) {
      announceStatusChanges(previous.segments, snapshot.segments);
    }
    state.initialized = true;
    return snapshot;
  }

  function closedSessionLabel(snapshot) {
    const status = text(snapshot.session.status).toLowerCase();
    const isArchived =
      status === "archived" ||
      snapshot.events.some((event) => {
        const source = asObject(event);
        return text(asObject(source.plan).kind).toLowerCase() === "archived";
      });
    if (isArchived) {
      return "Archived";
    }
    if (
      status === "closed" ||
      status === "complete" ||
      status === "completed" ||
      status === "inactive"
    ) {
      return "Closed";
    }
    return "";
  }

  function announceStatusChanges(previousSegments, nextSegments) {
    const previousStatuses = new Map(
      previousSegments.map((segment) => [segment.id, segment.status]),
    );
    const changes = nextSegments
      .filter(
        (segment) =>
          previousStatuses.has(segment.id) &&
          previousStatuses.get(segment.id) !== segment.status,
      )
      .map(
        (segment) =>
          `${segment.title} moved to ${STATUS_LABELS[segment.status]}.`,
      );
    if (changes.length > 0) {
      elements.screenReaderUpdates.textContent = changes.join(" ");
    }
  }

  function schedulePoll(delay = POLL_INTERVAL_MS) {
    window.clearTimeout(state.timer);
    if (state.terminal) {
      return;
    }
    state.timer = window.setTimeout(pollSnapshot, delay);
  }

  async function pollSnapshot() {
    if (state.requestInFlight) {
      schedulePoll();
      return;
    }

    state.requestInFlight = true;
    const headers = { Accept: "application/json" };
    if (state.etag) {
      headers["If-None-Match"] = state.etag;
    }

    try {
      const response = await fetch("/api/snapshot", {
        method: "GET",
        headers,
        cache: "no-store",
        credentials: "same-origin",
      });

      if (response.status === 304) {
        state.failures = 0;
        hideError();
        setConnection("live", "Live");
        return;
      }
      if (response.status === 410) {
        state.terminal = true;
        window.clearTimeout(state.timer);
        setConnection("offline", "Unavailable");
        showError(
          "This plan is no longer available.",
          "It may have been removed or the local plan server may no longer have access to its session.",
        );
        return;
      }
      if (!response.ok) {
        throw new Error(`Snapshot request returned HTTP ${response.status}.`);
      }

      const contentType = response.headers.get("content-type") || "";
      if (!contentType.toLowerCase().includes("application/json")) {
        throw new Error("Snapshot response was not JSON.");
      }

      const raw = await response.json();
      const nextEtag = response.headers.get("etag");
      if (nextEtag) {
        state.etag = nextEtag;
      }
      state.failures = 0;
      hideError();
      const snapshot = renderSnapshot(raw);
      const closedLabel = closedSessionLabel(snapshot);
      if (closedLabel) {
        state.terminal = true;
        elements.sessionKicker.textContent = `${closedLabel} session`;
        setConnection("closed", closedLabel);
      } else {
        setConnection("live", "Live");
      }
    } catch (error) {
      state.failures += 1;
      const reconnecting = state.snapshot !== null;
      setConnection("offline", reconnecting ? "Reconnecting" : "Offline");
      showError(
        reconnecting ? "Live updates paused." : "Unable to load this plan.",
        error instanceof Error
          ? `${error.message} Wookie will retry automatically.`
          : "Wookie will retry automatically.",
      );
    } finally {
      state.requestInFlight = false;
      schedulePoll();
    }
  }

  function renderDrawerSegment(segment) {
    const readiness = readinessFor(segment);
    elements.drawerStatus.textContent = `${STATUS_LABELS[segment.status]} · ${segment.id}`;
    elements.drawerTitle.textContent = segment.title;
    elements.drawerJustification.textContent = segment.justification;
    elements.drawerVerification.textContent = segment.verification;
    elements.drawerGuideId.textContent = segment.guide;
    elements.drawerReadiness.className =
      `readiness-line ${readiness.className}`.trim();
    elements.drawerReadiness.textContent = readiness.label;
    replaceList(
      elements.drawerDependencies,
      segment.dependsOn,
      "No dependencies recorded.",
    );
    replaceList(
      elements.drawerDecisions,
      segment.decisions,
      "No architectural decisions recorded.",
    );
  }

  function resetGuide() {
    elements.guidePage.hidden = true;
    elements.guideTitle.textContent = "";
    elements.guideDescription.textContent = "";
    elements.guideTags.replaceChildren();
    elements.guideBody.textContent = "";
    elements.guideState.hidden = false;
    elements.guideState.className = "guide-state";
  }

  function guideError(message, warning = false) {
    resetGuide();
    elements.guideState.classList.add(warning ? "is-warning" : "is-error");
    elements.guideState.textContent = message;
  }

  async function loadGuide(segmentId) {
    if (!segmentId) {
      guideError("This segment has no stable ID, so its guide cannot be loaded.");
      return;
    }

    if (state.guideRequest) {
      state.guideRequest.abort();
    }
    const controller = new AbortController();
    state.guideRequest = controller;
    const requestedSegmentId = segmentId;
    resetGuide();
    elements.guideReload.disabled = true;
    elements.guideState.textContent = "Loading the implementation guide…";

    try {
      const response = await fetch(
        `/api/guides/${encodeURIComponent(requestedSegmentId)}`,
        {
          headers: { Accept: "application/json" },
          cache: "no-store",
          credentials: "same-origin",
          signal: controller.signal,
        },
      );
      if (!response.ok) {
        throw new Error(
          response.status === 404
            ? "The implementation guide is missing or still a stub."
            : `The guide request returned HTTP ${response.status}.`,
        );
      }

      const contentType = response.headers.get("content-type") || "";
      if (!contentType.toLowerCase().includes("application/json")) {
        throw new Error("The guide response was not JSON.");
      }

      const payload = asObject(await response.json());
      if (
        state.selectedSegmentId !== requestedSegmentId ||
        !elements.dialog.open
      ) {
        return;
      }
      const page = asObject(payload.page);
      const pageId = text(page.id);
      const pageTitle = text(page.title, pageId);
      const body = text(page.body);
      const warning = text(payload.warning);

      if (!pageId || !pageTitle) {
        throw new Error("The guide response did not include a readable page.");
      }
      if (!body) {
        guideError(
          warning || "The implementation guide is empty or still a stub.",
          true,
        );
        return;
      }

      elements.guideTitle.textContent = pageTitle;
      elements.guideDescription.textContent = text(page.description);
      elements.guideDescription.hidden = !text(page.description);
      elements.guideTags.replaceChildren();
      stringList(page.tags).forEach((tag) => {
        elements.guideTags.append(createElement("li", "", tag));
      });
      elements.guideTags.hidden = stringList(page.tags).length === 0;
      elements.guideBody.textContent = body;
      elements.guidePage.hidden = false;

      if (warning) {
        elements.guideState.className = "guide-state is-warning";
        elements.guideState.textContent = warning;
        elements.guideState.hidden = false;
      } else {
        elements.guideState.hidden = true;
      }
    } catch (error) {
      if (error instanceof DOMException && error.name === "AbortError") {
        return;
      }
      guideError(
        error instanceof Error
          ? error.message
          : "The implementation guide could not be loaded.",
      );
    } finally {
      if (state.guideRequest === controller) {
        state.guideRequest = null;
        elements.guideReload.disabled = false;
      }
    }
  }

  function openSegment(segment, focusGuide) {
    state.selectedSegmentId = segment.id;
    renderDrawerSegment(segment);
    resetGuide();
    if (!elements.dialog.open) {
      elements.dialog.showModal();
    }
    loadGuide(segment.id);
    if (focusGuide) {
      elements.guideState.scrollIntoView({ block: "nearest" });
    }
  }

  function closeDrawer() {
    if (state.guideRequest) {
      state.guideRequest.abort();
      state.guideRequest = null;
    }
    if (elements.dialog.open) {
      elements.dialog.close();
    }
    state.selectedSegmentId = null;
  }

  elements.drawerClose.addEventListener("click", closeDrawer);
  elements.guideReload.addEventListener("click", () => {
    if (state.selectedSegmentId) {
      loadGuide(state.selectedSegmentId);
    }
  });
  elements.dialog.addEventListener("click", (event) => {
    if (event.target === elements.dialog) {
      closeDrawer();
    }
  });
  elements.dialog.addEventListener("close", () => {
    if (state.guideRequest) {
      state.guideRequest.abort();
      state.guideRequest = null;
    }
    state.selectedSegmentId = null;
  });

  setConnection("connecting", "Connecting");
  pollSnapshot();
})();
