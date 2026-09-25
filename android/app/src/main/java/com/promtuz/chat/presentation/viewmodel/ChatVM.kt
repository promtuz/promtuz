package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.graphics.Bitmap
import android.net.Uri
import android.os.SystemClock
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.domain.model.acceptsStaged
import com.promtuz.chat.domain.model.Activity
import com.promtuz.chat.domain.model.AlbumItem
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.SystemEventKind
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.domain.model.Quote
import com.promtuz.chat.domain.model.ReactionGroup
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.domain.model.StagedMedia
import com.promtuz.chat.domain.model.StickerRef
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.domain.model.mediaLabel
import com.promtuz.chat.domain.model.toRecord
import com.promtuz.chat.domain.model.toRef
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.chat.utils.media.VoicePlayer
import com.promtuz.chat.utils.media.VoiceRecorder
import com.promtuz.chat.utils.media.decodeAvifCached
import com.promtuz.chat.utils.media.videoPoster
import com.promtuz.chat.utils.media.decodeDownscaled
import com.promtuz.chat.utils.media.resolvePickedFile
import com.promtuz.chat.utils.media.toRgba
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import kotlinx.coroutines.FlowPreview
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.debounce
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.launch
import uniffi.core.MediaRecord
import uniffi.core.MessageRecord
import uniffi.core.ReactionRecord

/**
 * Reactive chat. [messages] observes the DB — re-read on every commit touching
 * messages/reactions — so send / receive / edit / delete / reaction / receipt all
 * surface as row updates with no hand-patching. [input] is the draft, cleared the
 * point the core accepts [send]. Rejected edits retain the draft. Newest message sits at
 * index 0 and the list draws reversed, so new messages land at the bottom. Typing
 * is an ephemeral signal, timed out client-side.
 */
class ChatVM(
    private val application: Application,
    private val app: AppVM,
    val conversationHex: String,
) : ViewModel() {
    /** The chat's scope — a 16-byte conversation id, group or 1:1 alike. */
    private val conversation = conversationHex.fromHex()
    // Seed before composition, without waiting for the initial database query.
    private val initialSummary = app.chats.value.firstOrNull { it.conversationHex == conversationHex }
    /**
     * Everyone in the chat except us. Presence and typing are per-person, so
     * they key off this rather than off the conversation: comparing a
     * conversation id against a peer IPK would simply never match.
     */
    private var others: List<ByteArray> = emptyList()
    private var started = false
    private var chatForeground = false
    private var lastMarkedRead: String? = null
    private val outgoingActivity = OutgoingActivity(viewModelScope, SystemClock::uptimeMillis) { activity ->
        CoreBridge.setActivity(conversation, activity)
    }
    fun setChatForeground(value: Boolean) {
        chatForeground = value
        outgoingActivity.setForeground(value)
        if (value) markVisibleMessagesRead()
    }

    private fun markVisibleMessagesRead() {
        if (!chatForeground) return
        val did = _messages.value.orEmpty().firstOrNull { !it.outgoing }?.dispatchIdHex ?: return
        if (did == lastMarkedRead) return
        lastMarkedRead = did
        fire { CoreBridge.markRead(conversation, did.fromHex()) }
    }

    /** True once the roster is bigger than a pair — drives per-sender bubbles. */
    private val _isGroup = MutableStateFlow(initialSummary?.isGroup == true)
    val isGroup: StateFlow<Boolean> = _isGroup.asStateFlow()

    /**
     * The group's own name, live. Empty for a 1:1, whose header name is the
     * contact's. The route carries a name too, but that one is a snapshot from
     * when the chat was opened — a rename has to land on an open header.
     */
    private val _title = MutableStateFlow(initialSummary?.name.orEmpty())
    val title: StateFlow<String> = _title.asStateFlow()

    /**
     * The name actually set on the group, blank until someone sets one. The
     * avatar wants this rather than [title]: an unnamed group draws its
     * members' initials, and a derived name would hide that it has none.
     */
    private val _rawTitle = MutableStateFlow(initialSummary?.rawTitle.orEmpty())
    val rawTitle: StateFlow<String> = _rawTitle.asStateFlow()

    /** Notifications silenced for this chat. A conversation flag, so it rides the row. */
    private val _muted = MutableStateFlow(initialSummary?.muted == true)
    val muted: StateFlow<Boolean> = _muted.asStateFlow()

    /**
     * Member IPK hex → display name, for attributing bubbles in a group.
     * Departed members stay in here — their old messages still need a name.
     */
    private val _memberNames = MutableStateFlow<Map<String, String>>(emptyMap())
    val memberNames: StateFlow<Map<String, String>> = _memberNames.asStateFlow()

    /** How many are in the group *now* — the header's count, so it excludes the departed. */
    private val _memberCount = MutableStateFlow(initialSummary?.memberCount ?: 0)
    val memberCount: StateFlow<Int> = _memberCount.asStateFlow()

    /** Who is currently typing, by member hex — a group can have several. */
    private val _typingMembers = MutableStateFlow(
        app.conversationActivity.members.value[conversationHex].orEmpty()
            .filterValues { Activity.Typing in Activity.fromBits(it) }.keys.toSet()
    )
    val typingMembers: StateFlow<Set<String>> = _typingMembers.asStateFlow()
    private val _typing = MutableStateFlow(_typingMembers.value.isNotEmpty())
    private val typingPresentation = TypingPresentation(viewModelScope)
    val typingBubbleMembers: StateFlow<Set<String>> = typingPresentation.members

    private fun syncTyping() {
        val people = app.conversationActivity.members.value[conversationHex].orEmpty()
        _typingMembers.value = people.filterValues { Activity.Typing in Activity.fromBits(it) }.keys
        _typing.value = _typingMembers.value.isNotEmpty()
        typingPresentation.update(_typingMembers.value)
    }

    // Null is still loading; an empty list is a loaded chat whose first new message can animate.
    private val _messages = MutableStateFlow<List<UiMessage>?>(null)
    val messages: StateFlow<List<UiMessage>?> = _messages.asStateFlow()

    /** Composer draft; two-way bound to the input field, cleared on [send]. */
    val input = MutableStateFlow("")

    /** Reply/edit staging shown as a chip above the composer; consumed by [send]. */
    val composerAction = MutableStateFlow<ComposerAction?>(null)

    private data class SavedDraft(val text: String, val reply: ComposerAction?)
    private var savedDraft: SavedDraft? = null
    private val editDrafts = mutableMapOf<String, String>()
    private val _sentRevision = MutableStateFlow(0L)
    val sentRevision: StateFlow<Long> = _sentRevision.asStateFlow()
    val composerBusy = MutableStateFlow(false)
    val composerError = MutableStateFlow<String?>(null)
    private var pickingMedia = false

    private fun restoreDraft() {
        val draft = savedDraft
        savedDraft = null
        editDrafts.clear()
        input.value = draft?.text.orEmpty()
        composerAction.value = draft?.reply
    }

    /**
     * The composer's media buffer, mirrored from libcore's staging registry.
     * Picking fills it and starts the encode; [send] commits it. While anything
     * here is still preparing the send is held — libcore refuses a half-encoded
     * item rather than dispatch a husk.
     */
    private val _staged = MutableStateFlow<List<StagedMedia>>(emptyList())
    val staged: StateFlow<List<StagedMedia>> = _staged.asStateFlow()

    /** Decoded tile per staged id — the client-side preview the core doesn't return. */
    private val previews = mutableMapOf<ULong, ImageBitmap>()

    /**
     * In-chat search. [searchQuery] is null while the bar is closed; hits are
     * newest first, and [hitIndex] walks them. A hit below the loaded window
     * widens the window first, then [jump] names the message for the screen
     * to glide to once its row exists.
     */
    val searchQuery = MutableStateFlow<String?>(null)
    private val _hits = MutableStateFlow<List<String>>(emptyList())
    val hits: StateFlow<List<String>> = _hits.asStateFlow()
    private var hitDepth: List<Int> = emptyList()
    private val _hitIndex = MutableStateFlow(0)
    val hitIndex: StateFlow<Int> = _hitIndex.asStateFlow()
    private val _jump = MutableSharedFlow<String>(extraBufferCapacity = 1)
    val jump: SharedFlow<String> = _jump.asSharedFlow()

    fun openSearch() { searchQuery.value = "" }
    fun closeSearch() { searchQuery.value = null; _hits.value = emptyList(); hitDepth = emptyList() }
    fun nextHit() = stepHit(1)
    fun prevHit() = stepHit(-1)

    /** Resolve against all stored history, then widen the same window used by search. */
    suspend fun jumpToDate(date: java.time.LocalDate, zone: java.time.ZoneId): Boolean {
        val start = date.atStartOfDay(zone).toEpochSecond()
        val position = CoreBridge.messageAtTime(conversation, start) ?: return false
        val dispatch = position.dispatchId?.toHex()
        limit = maxOf(limit, position.newer.toInt() + PAGE)
        var loaded = load()
        fun target() = loaded.firstOrNull { message ->
            message.localId == position.id || (dispatch != null &&
                (message.content as? MessageContent.Album)?.items?.any { it.dispatchIdHex == dispatch } == true)
        }
        // Concurrent arrivals can push the target below the depth we just read.
        // A folded album may also need its head from the preceding page.
        while (target() == null && !exhausted) {
            limit += PAGE
            loaded = load()
        }
        _messages.value = loaded
        val target = target() ?: return false
        _jump.emit(target.key)
        return true
    }

    private fun stepHit(by: Int) {
        val n = _hits.value.size
        if (n == 0) return
        val i = ((_hitIndex.value + by) % n + n) % n
        _hitIndex.value = i
        goToHit(i)
    }

    private fun goToHit(i: Int) {
        val did = _hits.value.getOrNull(i) ?: return
        // The depth was measured when the search ran; every message since
        // has pushed the hit one row deeper, so it is a floor, not the truth.
        // What decides is whether the row is actually loaded.
        val depth = hitDepth.getOrNull(i) ?: 0
        viewModelScope.launch {
            if (_messages.value.orEmpty().none { it.dispatchIdHex == did }) {
                limit = maxOf(limit, depth) + PAGE
                _messages.value = load()
            }
            _jump.tryEmit(did)
        }
    }

    /** A voice note being recorded: how long so far and how loud right now. */
    data class Recording(val elapsedMs: Long, val level: Float)

    private val recorder = VoiceRecorder(application)
    private val _recording = MutableStateFlow<Recording?>(null)
    val recording: StateFlow<Recording?> = _recording.asStateFlow()
    private var recordingTicker: Job? = null

    val typing: StateFlow<Boolean> = _typing.asStateFlow()

    /** Key of the incoming message that ended a live typing signal — the morph target. */
    val typingHandoff = MutableStateFlow<String?>(null)

    private val _presence = MutableStateFlow(initialSummary?.peerHex?.let { CoreBridge.presenceByPeer.value[it] })
    val presence: StateFlow<Presence?> = _presence.asStateFlow()

    /** The first resolved header is initialization, not an animated live update. */
    private val _headerReady = MutableStateFlow(false)
    val headerReady: StateFlow<Boolean> = _headerReady.asStateFlow()

    fun init() {
        if (started) return
        started = true
        syncTyping()

        var incomingLoaded = false
        var newestIncoming: String? = null
        var previousIncomingKeys = emptySet<String>()
        viewModelScope.launch {
            // Roster and messages ride one doorbell. Attribution reads the
            // roster, so resolving them in separate flows would let a message
            // render before the names it needs — and nothing re-attributes it
            // afterwards, so "Unknown" would stick until the next write.
            observeQuery(
                setOf(
                    "messages", "reactions", "message_media", "partials",
                    "conversations", "conversation_members", "contacts",
                ),
            ) {
                resolveRoster()
                load()
            }.collect { list ->
                // Their message just landed — if they were typing, it inherits the
                // typing bubble (morph). Handoff is set BEFORE the list so one
                // recomposition sees both.
                val newest = list.firstOrNull { !it.outgoing }
                if (incomingLoaded && newest != null && newest.key != newestIncoming && newest.key !in previousIncomingKeys) {
                    (newest.senderHex ?: others.singleOrNull()?.toHex())?.let {
                        app.conversationActivity.update(conversationHex, it, 0)
                        typingPresentation.consume(it)
                    }
                    syncTyping()
                    // An idle signal can beat the message. The stage may still
                    // have an exiting typing row to hand over; it decides whether
                    // that source is present rather than relying on this Boolean.
                    if (typingBubbleMembers.value.isEmpty()) typingHandoff.value = newest.key
                }
                newestIncoming = newest?.key
                previousIncomingKeys = list.filterNot { it.outgoing }.mapTo(HashSet()) { it.key }
                incomingLoaded = true
                _messages.value = list
                markVisibleMessagesRead()
            }
        }

        // The buffer is process-wide in libcore, so a chat opening inherits
        // whatever the last one left. Clear it rather than surface someone
        // else's pick as this conversation's draft.
        fire { CoreBridge.clearStaged() }
        @OptIn(FlowPreview::class)
        viewModelScope.launch {
            searchQuery.debounce(250).collect { q ->
                if (q.isNullOrBlank()) {
                    _hits.value = emptyList(); hitDepth = emptyList(); _hitIndex.value = 0
                    return@collect
                }
                val found = runCatching { CoreBridge.searchMessages(conversation, q) }.getOrDefault(emptyList())
                _hits.value = found.map { it.dispatchId.toHex() }
                hitDepth = found.map { it.newer.toInt() }
                _hitIndex.value = 0
                if (found.isNotEmpty()) goToHit(0)
            }
        }
        viewModelScope.launch {
            observeQuery(setOf("staging")) { CoreBridge.stagedItems() }.collect { records ->
                updateStaged(records)
            }
        }

        viewModelScope.launch {
            app.conversationActivity.members.collect { syncTyping() }
        }

        // Initial presence is resolved with the roster; subsequent signals stay live.
        // AppVM owns the process-wide subscription, including its cached snapshot.
        viewModelScope.launch {
            CoreBridge.presence
                .filter { sig -> others.any { it.contentEquals(sig.peer) } }
                .collect { sig -> if (!_isGroup.value) _presence.value = sig.presence }
        }

        viewModelScope.launch { input.collect(outgoingActivity::edited) }
        viewModelScope.launch {
            var previous = emptyMap<String, Presence>()
            CoreBridge.presenceByPeer.collect { current ->
                val returned = others.any { peer ->
                    val key = peer.toHex()
                    val before = previous[key]
                    val after = current[key]
                    (after == Presence.Online && before != Presence.Online) ||
                        (after is Presence.Idle && before != Presence.Online && before !is Presence.Idle)
                }
                previous = current
                if (returned) outgoingActivity.refresh()
            }
        }
        viewModelScope.launch {
            CoreBridge.connection.collect { connection ->
                if (connection == com.promtuz.chat.presentation.state.ConnectionState.Connected) {
                    outgoingActivity.refresh()
                }
            }
        }
    }

    @Volatile
    private var limit = INITIAL_LIMIT


    /** A load returned fewer rows than asked → all history is loaded. */
    @Volatile
    private var exhausted = false
    private var loadingOlder = false

    fun toggleMute() = com.promtuz.chat.data.ChatPrefs.toggleMute(conversationHex, !_muted.value)

    /** Who is in this chat and what to call them — read before every message pass. */
    private suspend fun resolveRoster() {
        val record = runCatching { CoreBridge.conversation(conversation) }.getOrNull() ?: return
        val roster = runCatching { CoreBridge.members(conversation) }.getOrNull() ?: return
        // Core resolves a member's name — address book, then what they call
        // themselves, then their key's head — so every screen agrees on the
        // order. Only "You" is ours to say: we are never in our own contacts.
        val names = roster.associate { m ->
            m.ipk.toHex() to if (m.me) "You" else m.name
        }
        // Publish together on Main so the first composed header is coherent.
        // A transient read failure keeps the last known header instead of clearing it.
        withContext(Dispatchers.Main.immediate) {
            _isGroup.value = record.kind.toInt() == 1
            _title.value = record.displayName
            _rawTitle.value = record.title
            _muted.value = record.muted
            others = record.others
            _memberNames.value = names
            _memberCount.value = roster.count { it.active }
            _presence.value = record.peer?.let { CoreBridge.presenceByPeer.value[it.toHex()] }
            syncTyping()
            _headerReady.value = true
        }
    }

    private suspend fun load(): List<UiMessage> {
        val want = limit
        val rows = CoreBridge.messages(conversation, want)                   // oldest-first
        exhausted = rows.size < want
        val byMsg = CoreBridge.reactions(conversation).groupBy { it.dispatchId.toHex() }
        val media = CoreBridge.getMedia(conversation).associateBy { it.dispatchId.toHex() }
        // Quote resolution: replies name a dispatch_id; snippet comes from the
        // loaded window (null text → "unavailable" shell, e.g. outside window).
        val byDid = rows.asSequence().mapNotNull { r -> r.dispatchId?.let { it.toHex() to r } }.toMap()
        // reversed → newest at index 0 → drawn at the bottom under reverseLayout;
        // AVIF decode happens in toUi, so map off the main thread.
        val loaded = withContext(Dispatchers.Default) {
            // "Seen by N" is only meaningful for our own messages in a group;
            // a 1:1 already says it with the delivery tick, so the per-message
            // count query is skipped entirely there.
            val seen: Map<String, Int> = if (!_isGroup.value) emptyMap() else
                rows.filter { it.outgoing && it.dispatchId != null }
                    .associate { r ->
                        val did = r.dispatchId!!
                        did.toHex() to runCatching {
                            CoreBridge.seenBy(conversation, did)
                        }.getOrDefault(0)
                    }
            // Core already folded runs of pictures sent together into albums;
            // the head row carries the run and the rest are marked to skip.
            rows.asReversed()
                .filterNot { it.inAlbum }
                .map { it.toUi(byMsg, byDid, media, _memberNames.value, _isGroup.value, seen) }
        }
        // A date/search jump can widen the window while this read is decoding.
        // Never publish that older, narrower snapshot over the requested window.
        return if (want < limit) load() else loaded
    }

    /**
     * Near-top pagination: grow the window and re-read. An accumulating beforeId
     * cursor would fight the reactive re-read (observeQuery reloads the whole
     * window on every commit); a bigger limit composes with it.
     * ponytail: grow-limit re-reads the full window per page — beforeId keyset
     * paging if that re-read ever gets too heavy.
     */
    fun loadOlder() {
        if (loadingOlder || exhausted) return
        loadingOlder = true
        limit += PAGE
        viewModelScope.launch {
            try {
                _messages.value = load()
            } finally {
                loadingOlder = false
            }
        }
    }

    /**
     * Commit the composer: buffered media (with the draft as its caption) if
     * there is any, plain text otherwise. Held while anything is still encoding
     * — libcore refuses a half-prepared item, so the UI keeps send disabled
     * until the buffer settles rather than letting it fail silently.
     */
    fun send() {
        if (composerBusy.value || pickingMedia) return
        val text = input.value.trim()
        val items = _staged.value
        val action = composerAction.value
        val editing = action as? ComposerAction.Edit
        val canClearCaption = editing?.msg?.content is MessageContent.Image ||
            editing?.msg?.content is MessageContent.Attachment
        if (text.isEmpty() && items.isEmpty() && !canClearCaption) return
        if (items.any { !it.ready }) return
        if (editing != null && (items.size > 1 || items.any { !editing.msg.content.acceptsStaged(it.kind) })) {
            composerError.value = "Choose one compatible replacement for this message."
            return
        }
        val did = editing?.msg?.dispatchIdHex
        if (editing != null && did == null) {
            composerError.value = "This message can’t be edited yet."
            return
        }
        if (editing != null && items.isEmpty() && text == editing.msg.editableText().trim()) {
            restoreDraft()
            return
        }
        composerBusy.value = true
        composerError.value = null
        viewModelScope.launch {
            try {
                when {
                    editing != null && items.isNotEmpty() -> {
                        CoreBridge.reviseWithStaged(conversation, did!!.fromHex(), items.single().id, text)
                        // Remove only the committed replacement, never an unrelated buffer.
                        runCatching { CoreBridge.discardStaged(items.single().id) }
                    }
                    editing != null -> CoreBridge.editMessage(conversation, did!!.fromHex(), text)
                    // An album holds at most ten; a bigger pick goes out as
                    // several albums, the caption and reply riding the first.
                    items.isNotEmpty() -> items.chunked(ALBUM_MAX).forEachIndexed { i, chunk ->
                        CoreBridge.sendStaged(conversation, chunk.map { it.id }, if (i == 0) text else "",
                            if (i == 0) (action as? ComposerAction.Reply)?.msg?.dispatchIdHex?.fromHex() else null)
                    }
                    else -> CoreBridge.sendMessage(conversation, text,
                        (action as? ComposerAction.Reply)?.msg?.dispatchIdHex?.fromHex())
                }
                _staged.value = _staged.value.filterNot { media -> items.any { it.id == media.id } }
                if (editing != null) restoreDraft()
                else { _sentRevision.value++; input.value = ""; composerAction.value = null }
            } catch (e: kotlinx.coroutines.CancellationException) {
                throw e
            } catch (e: Exception) {
                composerError.value = if (editing != null) "Couldn’t save changes. Try again." else "Couldn’t send. Try again."
            } finally { composerBusy.value = false }
        }
    }

    fun beginReply(msg: UiMessage) {
        if (composerBusy.value || recording.value != null || pickingMedia) return
        if (composerAction.value is ComposerAction.Edit) {
            if (_staged.value.isNotEmpty()) {
                composerError.value = "Remove the replacement before switching to a reply."
                return
            }
            restoreDraft()
        }
        composerError.value = null
        composerAction.value = ComposerAction.Reply(msg)
    }

    fun beginEdit(msg: UiMessage) {
        if (composerBusy.value || recording.value != null || pickingMedia) return
        if (composerAction.value?.msg?.key == msg.key && composerAction.value is ComposerAction.Edit) return
        if (_staged.value.isNotEmpty()) {
            composerError.value = "Send or remove the selected media before editing another message."
            return
        }
        val current = composerAction.value
        if (current is ComposerAction.Edit) editDrafts[current.msg.key] = input.value
        else savedDraft = SavedDraft(input.value, current)
        composerError.value = null
        composerAction.value = ComposerAction.Edit(msg)
        input.value = editDrafts[msg.key] ?: msg.editableText()
    }

    fun cancelComposerAction() {
        if (composerBusy.value || pickingMedia) return
        composerError.value = null
        if (composerAction.value is ComposerAction.Edit) {
            val replacements = _staged.value.map { it.id }
            if (replacements.isEmpty()) restoreDraft()
            else {
                composerBusy.value = true
                viewModelScope.launch {
                    try {
                        replacements.forEach { CoreBridge.discardStaged(it) }
                        updateStaged(CoreBridge.stagedItems())
                        restoreDraft()
                    } catch (e: kotlinx.coroutines.CancellationException) { throw e }
                    catch (e: Exception) { composerError.value = "Couldn’t remove the replacement. Try again." }
                    finally { composerBusy.value = false }
                }
            }
        } else composerAction.value = null
    }

    /** Tap on a quick-reaction or an existing chip: mine → remove, else add. */
    fun toggleReaction(msg: UiMessage, emoji: String) {
        val id = msg.dispatchIdHex ?: return
        val mine = msg.reactions.any { it.emoji == emoji && it.mine }
        react(id, emoji, add = !mine)
    }

    fun edit(dispatchIdHex: String, text: String) =
        fire { CoreBridge.editMessage(conversation, dispatchIdHex.fromHex(), text) }

    fun delete(dispatchIdHex: String, forEveryone: Boolean) =
        fire { CoreBridge.deleteMessage(conversation, dispatchIdHex.fromHex(), forEveryone) }

    fun react(dispatchIdHex: String, emoji: String, add: Boolean) =
        fire { CoreBridge.react(conversation, dispatchIdHex.fromHex(), emoji, add) }

    /**
     * Picked media → the composer buffer. The AVIF pass starts now and runs
     * while the caption is typed; [send] commits what's ready. Videos ride the
     * P2P attachment path. The album id is minted at commit time, so a pick
     * added later still joins the same group.
     */
    fun attachPhotos(uris: List<Uri>) = prepareMedia(uris, photos = true) { selected ->
        val cr = application.contentResolver
        selected.forEach { uri ->
            // ponytail: video staged raw over P2P — transcode + poster frame land later.
            if (cr.getType(uri)?.startsWith("video/") == true) stagePickedFile(uri)
            else {
                val bmp = decodeDownscaled(application, uri, INLINE_MAX_EDGE) ?: return@forEach
                val tile = bmp.tile()
                rememberPreview(CoreBridge.stageImage(bmp.toRgba(), bmp.width, bmp.height), tile)
            }
        }
    }

    /** A photo or clip the camera just wrote, as the picker would have staged it. */
    fun attachCaptured(file: java.io.File, video: Boolean) {
        val uri = Uri.fromFile(file)
        if (video) prepareMedia(listOf(uri), photos = false) { stageCaptured(file, "video/mp4", uri) }
        else prepareMedia(listOf(uri), photos = true) {
            val bmp = decodeDownscaled(application, uri, INLINE_MAX_EDGE) ?: return@prepareMedia
            rememberPreview(CoreBridge.stageImage(bmp.toRgba(), bmp.width, bmp.height), bmp.tile())
        }
    }

    private suspend fun stageCaptured(file: java.io.File, mime: String, uri: Uri) {
        val poster = videoPoster(application, uri, POSTER_MAX_EDGE)?.first
        val id = CoreBridge.stageAttachment(
            file.absolutePath, file.name, mime,
            poster?.toRgba(), poster?.width ?: 0, poster?.height ?: 0,
        )
        poster?.let { rememberPreview(id, it.tile()) }
    }

    /** Picked documents → the buffer as P2P attachments. */
    fun attachFiles(uris: List<Uri>) = prepareMedia(uris, photos = false) { selected -> selected.forEach { stagePickedFile(it) } }

    private fun prepareMedia(uris: List<Uri>, photos: Boolean, prepare: suspend (List<Uri>) -> Unit) {
        if (composerBusy.value || pickingMedia || uris.isEmpty()) return
        val editing = composerAction.value as? ComposerAction.Edit
        if (editing != null) {
            val kind = if (photos) com.promtuz.chat.domain.model.STAGED_IMAGE else com.promtuz.chat.domain.model.STAGED_ATTACHMENT
            if (!editing.msg.content.acceptsStaged(kind) || uris.size != 1 || _staged.value.isNotEmpty() ||
                (photos && uris.any { application.contentResolver.getType(it)?.startsWith("video/") == true })) {
                composerError.value = "Choose one compatible replacement for this message."
                return
            }
        }
        pickingMedia = true
        composerBusy.value = true
        composerError.value = null
        viewModelScope.launch {
            try { prepare(uris); updateStaged(CoreBridge.stagedItems()) }
            catch (e: kotlinx.coroutines.CancellationException) { throw e }
            catch (e: Exception) { composerError.value = "Couldn’t prepare the media. Try again." }
            finally { pickingMedia = false; composerBusy.value = false }
        }
    }

    private fun updateStaged(records: List<uniffi.core.StagedRecord>) {
        _staged.value = records.map { r ->
            StagedMedia(
                id = r.id,
                kind = r.kind.toInt(),
                state = r.state.toInt(),
                name = r.name,
                mime = r.mime,
                size = r.size.toLong(),
                width = r.width.toInt(),
                height = r.height.toInt(),
                // An image's tile is decoded from the pick at stage time; an
                // attachment's is libcore's blurred thumb, keyed per staged id.
                preview = previews[r.id]
                    ?: r.thumb?.let { decodeAvifCached("staged-${r.id}", it) },
                error = r.error,
            )
        }
        previews.keys.retainAll(records.map { it.id }.toSet())
    }

    /** Drop one buffered item; safe mid-encode. */
    fun unstage(id: ULong) = fire {
        if (composerBusy.value) return@fire
        previews.remove(id)
        CoreBridge.discardStaged(id)
    }

    /**
     * Start a voice note. The caller has the mic permission in hand; a false
     * return is the device refusing (another app holds the mic).
     */
    fun startRecording(): Boolean {
        if (composerBusy.value || pickingMedia || composerAction.value is ComposerAction.Edit || input.value.isNotBlank() || _staged.value.isNotEmpty()) return false
        if (recorder.isRecording) return true
        VoicePlayer.stop()
        if (!recorder.start(onLimit = { finishRecording() })) return false
        _recording.value = Recording(0, 0f)
        recordingTicker = viewModelScope.launch {
            while (recorder.isRecording) {
                _recording.value = Recording(recorder.elapsedMs, recorder.sample())
                delay(100)
            }
        }
        return true
    }

    fun cancelRecording() {
        recordingTicker?.cancel()
        recorder.cancel()
        _recording.value = null
    }

    /**
     * Stop and send. A recording too short to be a note is dropped, not sent.
     * A staged reply rides along and is consumed, as it would be by [send].
     */
    fun finishRecording() {
        recordingTicker?.cancel()
        val rec = recorder.finish()
        _recording.value = null
        val r = rec ?: return
        val to = conversation
        val replyTo = (composerAction.value as? ComposerAction.Reply)?.msg?.dispatchIdHex?.fromHex()
        composerAction.value = null
        fire { CoreBridge.sendVoice(to, r.bytes, r.mime, r.durationMs, r.waveform, replyTo) }
    }

    /** Send immediately with the current reply, preserving the text draft. */
    fun sendSticker(ref: StickerRef) {
        if (composerAction.value is ComposerAction.Edit) return
        val to = conversation
        val replyTo = (composerAction.value as? ComposerAction.Reply)?.msg?.dispatchIdHex?.fromHex()
        composerAction.value = null
        fire { CoreBridge.sendSticker(to, ref.toRecord(), replyTo) }
    }

    fun setChoosingSticker(active: Boolean) = outgoingActivity.setChoosingSticker(active)

    override fun onCleared() {
        cancelRecording()
    }

    /** Copy a picked uri into cache and buffer it as a P2P attachment; image mimes get a preview thumb. */
    private suspend fun stagePickedFile(uri: Uri) {
        val picked = resolvePickedFile(application, uri) ?: return
        val thumb = when {
            picked.mime.startsWith("image/") -> decodeDownscaled(application, uri, POSTER_MAX_EDGE)
            picked.mime.startsWith("video/") -> videoPoster(application, uri, POSTER_MAX_EDGE)?.first
            else -> null
        }
        val id = CoreBridge.stageAttachment(
            picked.path, picked.name, picked.mime,
            thumb?.toRgba(), thumb?.width ?: 0, thumb?.height ?: 0,
        )
        thumb?.let { rememberPreview(id, it.tile()) }
    }

    /**
     * Bind the decoded tile to its staged id. Staging rings the doorbell before
     * this runs, so the first re-read can land without one — patch the emitted
     * list too rather than leave a blank tile until the encode finishes.
     */
    private fun rememberPreview(id: ULong, tile: ImageBitmap) {
        previews[id] = tile
        _staged.value = _staged.value.map { if (it.id == id) it.copy(preview = tile) else it }
    }

    /** Downscale a decoded pick to a strip tile — the full-size bitmap is far
     *  more than a 60dp square needs, and a multi-pick would hold several. */
    private fun Bitmap.tile(): ImageBitmap {
        val longest = maxOf(width, height).coerceAtLeast(1)
        if (longest <= TILE_MAX_EDGE) return asImageBitmap()
        val k = TILE_MAX_EDGE.toFloat() / longest
        return Bitmap.createScaledBitmap(
            this, (width * k).toInt().coerceAtLeast(1), (height * k).toInt().coerceAtLeast(1), true,
        ).asImageBitmap()
    }

    fun download(fileIdHex: String) = fire { CoreBridge.downloadAttachment(fileIdHex.fromHex()) }

    private fun fire(block: suspend () -> Unit) = viewModelScope.launch { runCatching { block() } }

    private companion object {
        /** Cap the inline photo's longest edge so the AVIF pass lands under libcore's
         *  256KB budget. Over-budget picks fail in the buffer, where the strip can
         *  show it, rather than at send time. */
        const val INLINE_MAX_EDGE = 1600

        /** Attachment preview thumb; libcore blurs it, so tiny is plenty. */
        const val THUMB_MAX_EDGE = 256
        const val POSTER_MAX_EDGE = 640
        const val ALBUM_MAX = 10

        /** Composer strip tile; a 60dp square needs nothing like the full pick. */
        const val TILE_MAX_EDGE = 192

        /** First load window: a screenful + buffer. loadOlder() pages the rest on scroll. */
        const val INITIAL_LIMIT = 40

        /** Near-top page-in growth per [loadOlder]. */
        const val PAGE = 100
    }
}

/** What the next [ChatVM.send] means: a staged reply or an in-place edit. */
sealed interface ComposerAction {
    val msg: UiMessage

    data class Reply(override val msg: UiMessage) : ComposerAction
    data class Edit(override val msg: UiMessage) : ComposerAction
}


/**
 * What the composer edits for this message: its text, or a media body's caption.
 * Reading only [MessageContent.Text] here leaves the field empty for a picture,
 * and committing that empty field wipes the caption it should have loaded.
 */
fun UiMessage.editableText(): String = when (val c = content) {
    is MessageContent.Text -> c.text
    is MessageContent.Image -> c.caption
    is MessageContent.Attachment -> c.caption
    is MessageContent.Album -> c.caption
    // Not editable — a system or call row narrates something that already
    // happened, and a voice note or sticker carries no text at all.
    is MessageContent.System, is MessageContent.Call, is MessageContent.Voice,
    is MessageContent.Sticker -> ""
}


private fun MessageRecord.toUi(
    reactionsByMsg: Map<String, List<ReactionRecord>>,
    byDid: Map<String, MessageRecord>,
    mediaByDid: Map<String, MediaRecord>,
    memberNames: Map<String, String> = emptyMap(),
    isGroup: Boolean = false,
    seenBy: Map<String, Int> = emptyMap(),
): UiMessage {
    val didHex = dispatchId?.toHex()
    val reactions = didHex?.let { reactionsByMsg[it] }
        ?.groupBy { it.emoji }
        ?.map { (emoji, rs) -> ReactionGroup(emoji, rs.size, rs.any { it.mine }) }
        ?: emptyList()
    val quote = replyTo?.toHex()?.let { rtHex ->
        val quoted = byDid[rtHex]
        Quote(
            dispatchIdHex = rtHex,
            // A captionless picture quotes as "Photo", not as nothing.
            text = quoted?.takeIf { !it.deleted }?.content?.ifEmpty {
                mediaByDid[rtHex]?.let { mediaLabel(it.kind.toInt(), it.name) }.orEmpty()
            },
            outgoing = quoted?.outgoing ?: false,
        )
    }
    val senderHex = senderIpk?.toHex()
    val payload = when {
        // A call row: `content` is the outcome the engine wrote —
        // "answered:<seconds>" for a connected call, else a word.
        system.toInt() == 5 -> MessageContent.Call(
            outgoing = outgoing,
            durationSecs = content.removePrefix("answered:").toIntOrNull()
                .takeIf { content.startsWith("answered:") },
            missed = content == "missed",
        )
        // A system row's `content` carries its target: a member's hex for the
        // membership events, the new name for a rename.
        system.toInt() != 0 -> MessageContent.System(
            event = when (system.toInt()) {
                1 -> SystemEventKind.Added
                2 -> SystemEventKind.Left
                3 -> SystemEventKind.Removed
                else -> SystemEventKind.Titled
            },
            actor = senderHex?.let { memberNames[it] } ?: "Someone",
            target = if (system.toInt() == 4) content else memberNames[content] ?: "someone",
        )
        albumItems.size > 1 -> MessageContent.Album(
            caption = content,
            items = albumItems.asReversed().map { did ->
                val h = did.toHex()
                AlbumItem(h, mediaByDid[h]?.toContent(h, "") ?: MessageContent.Text(""))
            },
        )
        else -> didHex?.let { h -> mediaByDid[h]?.toContent(h, content) }
            ?: MessageContent.Text(content)
    }
    return UiMessage(
        key = didHex ?: id,
        localId = id,
        dispatchIdHex = didHex,
        content = payload,
        outgoing = outgoing,
        // Only a group needs to say who spoke; a 1:1 has one possible author.
        senderHex = senderHex.takeIf { isGroup && !outgoing },
        senderName = senderHex?.takeIf { isGroup && !outgoing }?.let { memberNames[it] },
        status = SendStatus.from(status.toInt()),
        edited = edited,
        deleted = deleted,
        timestampMs = timestamp.toLong() * 1000,
        reactions = reactions,
        quote = quote,
        seenBy = didHex?.let { seenBy[it] } ?: 0,
    )
}

/** kind: 1 = inline Image (blob), 3 = inline Voice (blob), 4 = Sticker (reference), else P2P Attachment (thumb + transfer progress). */
private fun MediaRecord.toContent(dispatchIdHex: String, caption: String): MessageContent =
    if (kind.toInt() == 1) MessageContent.Image(
        caption = caption,
        bitmap = blob?.let { decodeAvifCached(dispatchIdHex, it) },
        width = width.toInt(),
        height = height.toInt(),
    ) else if (kind.toInt() == 4) (
        // Older backup formats may omit the reference.
        sticker?.let { MessageContent.Sticker(it.toRef()) } ?: MessageContent.Text(mediaLabel(4))
    ) else if (kind.toInt() == 3) MessageContent.Voice(
        dispatchIdHex = dispatchIdHex,
        mime = mime,
        durationMs = durationMs.toInt(),
        waveform = thumb ?: ByteArray(0),
        bytes = blob ?: ByteArray(0),
    ) else MessageContent.Attachment(
        caption = caption,
        name = name,
        size = size.toLong(),
        mime = mime,
        thumb = thumb?.let { decodeAvifCached(dispatchIdHex, it) },
        fileIdHex = fileId?.toHex().orEmpty(),
        transferState = transferState.toInt(),
        transferHave = transferHave.toInt(),
        transferTotal = transferTotal.toInt(),
        localPath = localPath,
    )
