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
import com.promtuz.chat.domain.model.systemContent
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

/** [messages] is re-read on every change; the newest sits at index 0 and the list draws reversed. */
class ChatVM(
    private val application: Application,
    private val app: AppVM,
    val conversationHex: String,
) : ViewModel() {
    private val conversation = conversationHex.fromHex()
    // Seed before composition, without waiting for the initial database query.
    private val initialSummary = app.chats.value.firstOrNull { it.conversationHex == conversationHex }
    /** Everyone but us; presence and typing key off people, not the conversation. */
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
        val did = _messages.value.orEmpty().firstOrNull {
            !it.outgoing && it.content !is MessageContent.System
        }?.dispatchIdHex ?: return
        if (did == lastMarkedRead) return
        fire {
            CoreBridge.markRead(conversation, did.fromHex())
            lastMarkedRead = did
        }
    }

    private val _isGroup = MutableStateFlow(initialSummary?.isGroup == true)
    val isGroup: StateFlow<Boolean> = _isGroup.asStateFlow()

    /** Live, unlike the route's name, so a rename lands on an open header. */
    private val _title = MutableStateFlow(initialSummary?.name.orEmpty())
    val title: StateFlow<String> = _title.asStateFlow()

    /** Blank until someone names the group, so the avatar draws the members' initials instead. */
    private val _rawTitle = MutableStateFlow(initialSummary?.rawTitle.orEmpty())
    val rawTitle: StateFlow<String> = _rawTitle.asStateFlow()

    private val _muted = MutableStateFlow(initialSummary?.muted == true)
    val muted: StateFlow<Boolean> = _muted.asStateFlow()

    /** Why we can't post here, or null; the composer gives way to it. */
    private val _closed = MutableStateFlow<String?>(null)
    val closed: StateFlow<String?> = _closed.asStateFlow()

    private val _request = MutableStateFlow(initialSummary?.request == true)
    val request: StateFlow<Boolean> = _request.asStateFlow()
    private var peer: ByteArray? = null

    fun acceptRequest() = viewModelScope.launch {
        val who = peer ?: return@launch
        runCatching { CoreBridge.acceptMessageRequest(who) }
            .onSuccess { _request.value = false }
            .onFailure { composerError.value = "Couldn't accept the request" }
    }

    fun dismissRequest(block: Boolean, onGone: () -> Unit) = viewModelScope.launch {
        val who = peer ?: return@launch
        runCatching { if (block) CoreBridge.blockMessageRequest(who) else CoreBridge.deleteMessageRequest(who) }
            .onSuccess { onGone() }
            .onFailure { composerError.value = if (block) "Couldn't block" else "Couldn't delete the request" }
    }

    /** Keyed by member hex, departed members included so old messages still have a name. */
    private val _memberNames = MutableStateFlow<Map<String, String>>(emptyMap())
    val memberNames: StateFlow<Map<String, String>> = _memberNames.asStateFlow()

    /** Excludes departed members, unlike [memberNames]. */
    private val _memberCount = MutableStateFlow(initialSummary?.memberCount ?: 0)
    val memberCount: StateFlow<Int> = _memberCount.asStateFlow()

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

    val input = MutableStateFlow("")

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

    /** Mirrors core's staging. Send waits while an item still prepares, since core refuses a half-encoded one. */
    private val _staged = MutableStateFlow<List<StagedMedia>>(emptyList())
    val staged: StateFlow<List<StagedMedia>> = _staged.asStateFlow()

    /** The buffer is shared with the share sheet and other chats; only these ids are ours. */
    private val owned = mutableSetOf<ULong>()

    private val previews = mutableMapOf<ULong, ImageBitmap>()

    /** Null while the bar is closed. A hit below the loaded window widens it before [jump] names the row. */
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

    suspend fun revealMessage(dispatchId: String, timestampMs: Long): String? {
        fun target(rows: List<UiMessage>) = rows.firstOrNull { message ->
            !message.deleted && (message.dispatchIdHex == dispatchId ||
                (message.content as? MessageContent.Album)?.items?.any { it.dispatchIdHex == dispatchId } == true)
        }
        target(_messages.value.orEmpty())?.let { return it.key }
        val position = CoreBridge.messageAtTime(conversation, timestampMs / 1000) ?: return null
        limit = maxOf(limit, position.newer.toInt() + PAGE)
        var loaded = load()
        while (target(loaded) == null && !exhausted) {
            limit += PAGE
            loaded = load()
        }
        _messages.value = loaded
        return target(loaded)?.key
    }

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
        // The depth was measured at search time and newer messages push the hit deeper, so it is a floor.
        val depth = hitDepth.getOrNull(i) ?: 0
        viewModelScope.launch {
            if (_messages.value.orEmpty().none { it.dispatchIdHex == did }) {
                limit = maxOf(limit, depth) + PAGE
                _messages.value = load()
            }
            _jump.tryEmit(did)
        }
    }

    data class Recording(val elapsedMs: Long, val level: Float)

    private val recorder = VoiceRecorder(application)
    private val _recording = MutableStateFlow<Recording?>(null)
    val recording: StateFlow<Recording?> = _recording.asStateFlow()
    private var recordingTicker: Job? = null

    val typing: StateFlow<Boolean> = _typing.asStateFlow()

    /** The incoming message that ended a typing signal, which the typing bubble morphs into. */
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
            // One query for roster and messages: attribution reads the roster, and nothing
            // re-attributes a message that rendered before its names.
            observeQuery(
                setOf(
                    "messages", "reactions", "message_media", "partials",
                    "conversations", "conversation_members", "contacts",
                ),
            ) {
                resolveRoster()
                load()
            }.collect { list ->
                // Their message inherits the typing bubble. The handoff is set before the list so
                // one recomposition sees both.
                val newest = list.firstOrNull { !it.outgoing }
                if (incomingLoaded && newest != null && newest.key != newestIncoming && newest.key !in previousIncomingKeys) {
                    (newest.senderHex ?: others.singleOrNull()?.toHex())?.let {
                        app.conversationActivity.update(conversationHex, it, 0)
                        typingPresentation.consume(it)
                    }
                    syncTyping()
                    // An idle signal can beat the message; the stage decides whether an exiting
                    // typing row is still there to hand over.
                    if (typingBubbleMembers.value.isEmpty()) typingHandoff.value = newest.key
                }
                newestIncoming = newest?.key
                previousIncomingKeys = list.filterNot { it.outgoing }.mapTo(HashSet()) { it.key }
                incomingLoaded = true
                _messages.value = list
                markVisibleMessagesRead()
            }
        }

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

        // Initial presence comes with the roster; AppVM owns the process-wide subscription.
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


    @Volatile
    private var exhausted = false
    private var loadingOlder = false

    fun toggleMute() = com.promtuz.chat.data.ChatPrefs.toggleMute(conversationHex, !_muted.value)

    private suspend fun resolveRoster() {
        val record = runCatching { CoreBridge.conversation(conversation) }.getOrNull() ?: return
        val roster = runCatching { CoreBridge.members(conversation) }.getOrNull() ?: return
        // Core resolves every name but ours: we are never in our own contacts.
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
            _request.value = record.request
            _closed.value = when {
                record.kind.toInt() != 1 -> null
                !record.amMember -> "You’re no longer in this group"
                !record.canSend -> "Only admins can send messages"
                else -> null
            }
            peer = record.peer
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
        val media = CoreBridge.getMedia(conversation, want).associateBy { it.dispatchId.toHex() }
        // Quotes resolve within the loaded window; one outside it shows as unavailable.
        val byDid = rows.asSequence().mapNotNull { r -> r.dispatchId?.let { it.toHex() to r } }.toMap()
        // toUi decodes AVIF, so this maps off the main thread.
        val loaded = withContext(Dispatchers.Default) {
            // Core folds pictures sent together onto the album's head row and marks the rest.
            rows.asReversed()
                .filterNot { it.inAlbum }
                .map { it.toUi(byMsg, byDid, media, _memberNames.value, _isGroup.value) }
        }
        // A date/search jump can widen the window while this read is decoding.
        // Never publish that older, narrower snapshot over the requested window.
        return if (want < limit) load() else loaded
    }

    /** Grows the window instead of paging by cursor, since every commit re-reads the whole window. */
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
                    // A bigger pick goes out as several albums, the caption and reply riding the first.
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

    fun toggleReaction(msg: UiMessage, emoji: String) {
        if (_request.value) { composerError.value = "Accept the request to react"; return }
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

    /** The album id is minted at send time, so a later pick still joins the same album. */
    fun attachPhotos(uris: List<Uri>) = prepareMedia(uris, photos = true) { selected ->
        val cr = application.contentResolver
        selected.forEach { uri ->
            if (cr.getType(uri)?.startsWith("video/") == true) stagePickedFile(uri)
            else {
                val bmp = decodeDownscaled(application, uri, INLINE_MAX_EDGE) ?: return@forEach
                val tile = bmp.tile()
                rememberPreview(own(CoreBridge.stageImage(bmp.toRgba(), bmp.width, bmp.height)), tile)
            }
        }
    }

    fun attachCaptured(file: java.io.File, video: Boolean) {
        val uri = Uri.fromFile(file)
        if (video) prepareMedia(listOf(uri), photos = false) { stageCaptured(file, "video/mp4", uri) }
        else prepareMedia(listOf(uri), photos = true) {
            val bmp = decodeDownscaled(application, uri, INLINE_MAX_EDGE) ?: return@prepareMedia
            rememberPreview(own(CoreBridge.stageImage(bmp.toRgba(), bmp.width, bmp.height)), bmp.tile())
        }
    }

    private suspend fun stageCaptured(file: java.io.File, mime: String, uri: Uri) {
        val poster = videoPoster(application, uri, POSTER_MAX_EDGE)?.first
        val id = own(CoreBridge.stageAttachment(
            file.absolutePath, file.name, mime,
            poster?.toRgba(), poster?.width ?: 0, poster?.height ?: 0,
        ))
        poster?.let { rememberPreview(id, it.tile()) }
    }

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

    private fun own(id: ULong): ULong {
        owned += id
        return id
    }

    private fun updateStaged(records: List<uniffi.core.StagedRecord>) {
        _staged.value = records.filter { it.id in owned }.map { r ->
            StagedMedia(
                id = r.id,
                kind = r.kind.toInt(),
                state = r.state.toInt(),
                name = r.name,
                mime = r.mime,
                size = r.size.toLong(),
                width = r.width.toInt(),
                height = r.height.toInt(),
                // An image's tile comes from the pick; an attachment's is core's blurred thumb.
                preview = previews[r.id]
                    ?: r.thumb?.let { decodeAvifCached("staged-${r.id}", it) },
                error = r.error,
            )
        }
        val live = records.map { it.id }.toSet()
        previews.keys.retainAll(live)
        owned.retainAll(live)
    }

    fun unstage(id: ULong) = fire {
        if (composerBusy.value) return@fire
        previews.remove(id)
        owned -= id
        CoreBridge.discardStaged(id)
    }

    /** The caller already holds the mic permission. */
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
        // The scope is gone by now; what this chat staged and never sent goes with it.
        val mine = owned.toList()
        if (mine.isNotEmpty()) {
            kotlinx.coroutines.CoroutineScope(Dispatchers.IO).launch {
                mine.forEach { runCatching { CoreBridge.discardStaged(it) } }
            }
        }
    }

    private suspend fun stagePickedFile(uri: Uri) {
        val picked = resolvePickedFile(application, uri) ?: return
        val thumb = when {
            picked.mime.startsWith("image/") -> decodeDownscaled(application, uri, POSTER_MAX_EDGE)
            picked.mime.startsWith("video/") -> videoPoster(application, uri, POSTER_MAX_EDGE)?.first
            else -> null
        }
        val id = own(CoreBridge.stageAttachment(
            picked.path, picked.name, picked.mime,
            thumb?.toRgba(), thumb?.width ?: 0, thumb?.height ?: 0,
        ))
        thumb?.let { rememberPreview(id, it.tile()) }
    }

    /** Staging rings the doorbell before this runs, so the emitted list is patched too. */
    private fun rememberPreview(id: ULong, tile: ImageBitmap) {
        previews[id] = tile
        _staged.value = _staged.value.map { if (it.id == id) it.copy(preview = tile) else it }
    }

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
        /** Keeps the AVIF pass under core's 256KB inline budget; an over-budget pick fails in the strip. */
        const val INLINE_MAX_EDGE = 1600

        const val POSTER_MAX_EDGE = 640
        const val ALBUM_MAX = 10

        /** Composer strip tile; a 60dp square needs nothing like the full pick. */
        const val TILE_MAX_EDGE = 192

        const val INITIAL_LIMIT = 40

        const val PAGE = 100
    }
}

sealed interface ComposerAction {
    val msg: UiMessage

    data class Reply(override val msg: UiMessage) : ComposerAction
    data class Edit(override val msg: UiMessage) : ComposerAction
}


/** A media body's caption counts too, or committing the empty field would wipe it. */
fun UiMessage.editableText(): String = when (val c = content) {
    is MessageContent.Text -> c.text
    is MessageContent.Image -> c.caption
    is MessageContent.Attachment -> c.caption
    is MessageContent.Album -> c.caption
    is MessageContent.System, is MessageContent.Call, is MessageContent.Voice,
    is MessageContent.Sticker -> ""
}


private fun MessageRecord.toUi(
    reactionsByMsg: Map<String, List<ReactionRecord>>,
    byDid: Map<String, MessageRecord>,
    mediaByDid: Map<String, MediaRecord>,
    memberNames: Map<String, String> = emptyMap(),
    isGroup: Boolean = false,
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
            text = quoted?.takeIf { !it.deleted }?.content?.ifEmpty {
                mediaByDid[rtHex]?.let { mediaLabel(it.kind.toInt(), it.name) }.orEmpty()
            },
        )
    }
    val senderHex = senderIpk?.toHex()
    val payload = when {
        // A call row's content is "answered:<seconds>" for a connected call, else a word such as "missed".
        system.toInt() == 5 -> MessageContent.Call(
            outgoing = outgoing,
            durationSecs = content.removePrefix("answered:").toIntOrNull()
                .takeIf { content.startsWith("answered:") },
            missed = content == "missed",
        )
        system.toInt() != 0 -> systemContent(system.toInt(), senderHex, content, memberNames)
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
        senderHex = senderHex.takeIf { isGroup && !outgoing },
        senderName = senderHex?.takeIf { isGroup && !outgoing }?.let { memberNames[it] },
        status = SendStatus.from(status.toInt()),
        edited = edited,
        deleted = deleted,
        timestampMs = timestamp.toLong() * 1000,
        reactions = reactions,
        quote = quote,
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
