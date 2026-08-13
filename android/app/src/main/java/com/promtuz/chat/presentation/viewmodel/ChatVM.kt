package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.graphics.Bitmap
import android.net.Uri
import android.os.SystemClock
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.domain.model.Activity
import com.promtuz.chat.domain.model.AlbumItem
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.SystemEventKind
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.domain.model.Quote
import com.promtuz.chat.domain.model.ReactionGroup
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.domain.model.StagedMedia
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.chat.utils.media.decodeAvifCached
import com.promtuz.chat.utils.media.decodeDownscaled
import com.promtuz.chat.utils.media.resolvePickedFile
import com.promtuz.chat.utils.media.toRgba
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.launch
import uniffi.core.MediaRecord
import uniffi.core.MessageRecord
import uniffi.core.ReactionRecord

/**
 * Reactive chat. [messages] observes the DB — re-read on every commit touching
 * messages/reactions — so send / receive / edit / delete / reaction / receipt all
 * surface as row updates with no hand-patching. [input] is the draft, cleared the
 * instant [send] fires (so the editor empties immediately). Newest message sits at
 * index 0 and the list draws reversed, so new messages land at the bottom. Typing
 * is an ephemeral signal, timed out client-side.
 */
class ChatVM(private val application: Application) : ViewModel() {
    /** The chat's scope — a 16-byte conversation id, group or 1:1 alike. */
    private var conversation: ByteArray = ByteArray(16)
    /**
     * Everyone in the chat except us. Presence and typing are per-person, so
     * they key off this rather than off the conversation: comparing a
     * conversation id against a peer IPK would simply never match.
     */
    private var others: List<ByteArray> = emptyList()
    private var started = false

    // Computed, not `by lazy`: the top bar composes before [init] runs and
    // reads this, and a lazy would memoize the placeholder zeros for the
    // lifetime of the VM — muting and Group info would both address nothing.
    val conversationHex: String get() = conversation.toHex()

    /** True once the roster is bigger than a pair — drives per-sender bubbles. */
    private val _isGroup = MutableStateFlow(false)
    val isGroup: StateFlow<Boolean> = _isGroup.asStateFlow()

    /**
     * The group's own name, live. Empty for a 1:1, whose header name is the
     * contact's. The route carries a name too, but that one is a snapshot from
     * when the chat was opened — a rename has to land on an open header.
     */
    private val _title = MutableStateFlow("")
    val title: StateFlow<String> = _title.asStateFlow()

    /**
     * The name actually set on the group, blank until someone sets one. The
     * avatar wants this rather than [title]: an unnamed group draws its
     * members' initials, and a derived name would hide that it has none.
     */
    private val _rawTitle = MutableStateFlow("")
    val rawTitle: StateFlow<String> = _rawTitle.asStateFlow()

    /**
     * Member IPK hex → display name, for attributing bubbles in a group.
     * Departed members stay in here — their old messages still need a name.
     */
    private val _memberNames = MutableStateFlow<Map<String, String>>(emptyMap())
    val memberNames: StateFlow<Map<String, String>> = _memberNames.asStateFlow()

    /** How many are in the group *now* — the header's count, so it excludes the departed. */
    private val _memberCount = MutableStateFlow(0)
    val memberCount: StateFlow<Int> = _memberCount.asStateFlow()

    /** Who is currently typing, by member hex — a group can have several. */
    private val _typingMembers = MutableStateFlow<Set<String>>(emptySet())
    val typingMembers: StateFlow<Set<String>> = _typingMembers.asStateFlow()

    private val _messages = MutableStateFlow<List<UiMessage>>(emptyList())
    val messages: StateFlow<List<UiMessage>> = _messages.asStateFlow()

    /** Composer draft; two-way bound to the input field, cleared on [send]. */
    val input = MutableStateFlow("")

    /** Reply/edit staging shown as a chip above the composer; consumed by [send]. */
    val composerAction = MutableStateFlow<ComposerAction?>(null)

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

    private val _typing = MutableStateFlow(false)
    val typing: StateFlow<Boolean> = _typing.asStateFlow()
    private var typingExpiry: Job? = null

    /** Key of the incoming message that ended a live typing signal — the morph target. */
    val typingHandoff = MutableStateFlow<String?>(null)

    private val _presence = MutableStateFlow<Presence?>(null)
    val presence: StateFlow<Presence?> = _presence.asStateFlow()

    fun init(conversationId: ByteArray) {
        if (started) return
        started = true
        conversation = conversationId

        var newestIncoming: String? = null
        var lastMarkedRead: String? = null
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
                if (newest?.key != newestIncoming) {
                    newestIncoming = newest?.key
                    if (_typing.value && newest != null) typingHandoff.value = newest.key
                    clearTyping()
                }
                // With this chat on screen it's read: receipt the high-water mark.
                // Keyed on the dispatch id, not the row — `key` falls back to the
                // local ULID, so a row can surface before the id the receipt needs.
                newest?.dispatchIdHex?.let { did ->
                    if (did != lastMarkedRead) {
                        lastMarkedRead = did
                        fire { CoreBridge.markRead(conversation, did.fromHex()) }
                    }
                }
                _messages.value = list
            }
        }

        // The buffer is process-wide in libcore, so a chat opening inherits
        // whatever the last one left. Clear it rather than surface someone
        // else's pick as this conversation's draft.
        fire { CoreBridge.clearStaged() }
        viewModelScope.launch {
            observeQuery(setOf("staging")) { CoreBridge.stagedItems() }.collect { records ->
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
        }

        viewModelScope.launch {
            CoreBridge.activity.filter { it.conversation.contentEquals(conversation) }.collect { sig ->
                val who = sig.peer.toHex()
                if (Activity.Typing in Activity.fromBits(sig.bits)) {
                    _typingMembers.value = _typingMembers.value + who
                    _typing.value = true
                    typingExpiry?.cancel()
                    typingExpiry = viewModelScope.launch {
                        delay(TYPING_TTL_MS)
                        _typingMembers.value = _typingMembers.value - who
                        _typing.value = _typingMembers.value.isNotEmpty()
                    }
                } else {
                    _typingMembers.value = _typingMembers.value - who
                    if (_typingMembers.value.isEmpty()) clearTyping()
                }
            }
        }

        // Seed from the app-wide cache (AppVM subscribes presence for all
        // contacts; a delta may have landed before this chat opened), then
        // track live. Subscription itself is owned by AppVM — not re-expressed
        // here, or the relay's full-set replace would narrow us to one peer.
        // A group has no single "last seen", so presence stays null there.
        viewModelScope.launch {
            observeQuery(setOf("conversations", "conversation_members")) {
                runCatching { CoreBridge.conversation(conversation)?.peer }.getOrNull()
            }.collect { peer ->
                if (peer == null) return@collect
                _presence.value = CoreBridge.presenceByPeer.value[peer.toHex()]
            }
        }
        viewModelScope.launch {
            CoreBridge.presence
                .filter { sig -> others.any { it.contentEquals(sig.peer) } }
                .collect { sig -> if (!_isGroup.value) _presence.value = sig.presence }
        }

        // Outbound typing: refresh under the peer's TTL while keystrokes flow,
        // one idle signal when the draft empties (send() clears input → same path).
        var lastSentAt = 0L
        viewModelScope.launch {
            input.collect { text ->
                if (text.isEmpty()) {
                    if (lastSentAt != 0L) {
                        lastSentAt = 0L
                        runCatching { CoreBridge.setActivity(conversation, 0) }
                    }
                } else {
                    val now = SystemClock.uptimeMillis()
                    if (now - lastSentAt >= TYPING_RESEND_MS) {
                        lastSentAt = now
                        runCatching { CoreBridge.setActivity(conversation, Activity.Typing.bit) }
                    }
                }
            }
        }
    }

    private fun clearTyping() {
        typingExpiry?.cancel()
        _typing.value = false
    }

    @Volatile
    private var limit = INITIAL_LIMIT


    /** A load returned fewer rows than asked → all history is loaded. */
    @Volatile
    private var exhausted = false
    private var loadingOlder = false

    /** Who is in this chat and what to call them — read before every message pass. */
    private suspend fun resolveRoster() {
        val record = runCatching { CoreBridge.conversation(conversation) }.getOrNull()
        val roster = runCatching { CoreBridge.members(conversation) }.getOrDefault(emptyList())
        val contacts = runCatching { CoreBridge.contacts() }.getOrDefault(emptyList())
            .associate { it.ipk.toHex() to it.name }
        _isGroup.value = record?.kind?.toInt() == 1
        _title.value = record?.displayName.orEmpty()
        _rawTitle.value = record?.title.orEmpty()
        others = record?.others.orEmpty()
        // We are never in our own address book, so name that row here or every
        // system line we author reads "Unknown".
        _memberNames.value = roster.associate { m ->
            val hex = m.ipk.toHex()
            hex to if (m.me) "You" else contacts[hex] ?: "Unknown"
        }
        _memberCount.value = roster.count { it.active }
    }

    private suspend fun load(): List<UiMessage> {
        val want = limit
        val rows = CoreBridge.messages(conversation, want)                   // oldest-first
        if (rows.size < want) exhausted = true
        val byMsg = CoreBridge.reactions(conversation).groupBy { it.dispatchId.toHex() }
        val media = CoreBridge.getMedia(conversation).associateBy { it.dispatchId.toHex() }
        // Quote resolution: replies name a dispatch_id; snippet comes from the
        // loaded window (null text → "unavailable" shell, e.g. outside window).
        val byDid = rows.asSequence().mapNotNull { r -> r.dispatchId?.let { it.toHex() to r } }.toMap()
        // reversed → newest at index 0 → drawn at the bottom under reverseLayout;
        // AVIF decode happens in toUi, so map off the main thread.
        return withContext(Dispatchers.Default) {
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
            val ui = rows.asReversed()
                .map { it.toUi(byMsg, byDid, media, _memberNames.value, _isGroup.value, seen) }
            collapseAlbums(ui) { did -> media[did]?.groupId?.toHex() }
        }
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
        val text = input.value.trim()
        val items = _staged.value
        if (text.isEmpty() && items.isEmpty()) return
        if (items.any { !it.ready }) return

        val action = composerAction.value
        input.value = ""
        composerAction.value = null

        if (items.isNotEmpty()) {
            val ids = items.map { it.id }
            when (action) {
                // A revision targets one message, so only the first pick can land.
                // The buffer isn't drained by the revise — body_of leaves items in
                // place so a refused swap doesn't cost the user their pick — so
                // clear it here, once the body is already on its way.
                is ComposerAction.Edit -> action.msg.dispatchIdHex?.let { did ->
                    fire {
                        CoreBridge.reviseWithStaged(conversation, did.fromHex(), ids.first(), text)
                        CoreBridge.clearStaged()
                    }
                }
                else -> {
                    val replyTo = (action as? ComposerAction.Reply)?.msg?.dispatchIdHex?.fromHex()
                    fire { CoreBridge.sendStaged(conversation, ids, text, replyTo) }
                }
            }
            return
        }

        when (action) {
            // An unchanged edit is dropped, not sent: apply_edit flags `edited = 1`
            // unconditionally, so firing one would stamp the message for nothing.
            is ComposerAction.Edit -> {
                val original = action.msg.editableText().trim()
                if (text != original) action.msg.dispatchIdHex?.let { edit(it, text) }
            }
            is ComposerAction.Reply -> fire {
                CoreBridge.sendMessage(conversation, text, action.msg.dispatchIdHex?.fromHex())
            }
            null -> fire { CoreBridge.sendMessage(conversation, text) }
        }
    }

    fun beginReply(msg: UiMessage) {
        composerAction.value = ComposerAction.Reply(msg)
    }

    fun beginEdit(msg: UiMessage) {
        composerAction.value = ComposerAction.Edit(msg)
        input.value = msg.editableText()
    }

    fun cancelComposerAction() {
        if (composerAction.value is ComposerAction.Edit) input.value = ""
        composerAction.value = null
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
    fun attachPhotos(uris: List<Uri>) = fire {
        val cr = application.contentResolver
        uris.forEach { uri ->
            // ponytail: video staged raw over P2P — transcode + poster frame land later.
            if (cr.getType(uri)?.startsWith("video/") == true) stagePickedFile(uri)
            else {
                val bmp = decodeDownscaled(application, uri, INLINE_MAX_EDGE) ?: return@forEach
                val tile = bmp.tile()
                rememberPreview(CoreBridge.stageImage(bmp.toRgba(), bmp.width, bmp.height), tile)
            }
        }
    }

    /** Picked documents → the buffer as P2P attachments. */
    fun attachFiles(uris: List<Uri>) = fire { uris.forEach { stagePickedFile(it) } }

    /** Drop one buffered item; safe mid-encode. */
    fun unstage(id: ULong) = fire {
        previews.remove(id)
        CoreBridge.discardStaged(id)
    }

    /** Copy a picked uri into cache and buffer it as a P2P attachment; image mimes get a preview thumb. */
    private suspend fun stagePickedFile(uri: Uri) {
        val picked = resolvePickedFile(application, uri) ?: return
        val thumb = if (picked.mime.startsWith("image/")) decodeDownscaled(application, uri, THUMB_MAX_EDGE) else null
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
        const val TYPING_TTL_MS = 6_000L

        /** Outbound refresh cadence; must stay under the peer's [TYPING_TTL_MS]. */
        const val TYPING_RESEND_MS = 4_000L

        /** Cap the inline photo's longest edge so the AVIF pass lands under libcore's
         *  256KB budget. Over-budget picks fail in the buffer, where the strip can
         *  show it, rather than at send time. */
        const val INLINE_MAX_EDGE = 1600

        /** Attachment preview thumb; libcore blurs it, so tiny is plenty. */
        const val THUMB_MAX_EDGE = 256

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
 * Fold runs of media sharing a `group_id` into one [MessageContent.Album] row.
 *
 * [messages] is newest-first, so a run reads newest→oldest and the album's items
 * are re-reversed into the order they were picked. The newest member represents
 * the row — it owns the position the album already occupies, which is what keeps
 * a late addition from re-sorting the conversation — while the caption is lifted
 * from whichever member carries it (the first sent).
 *
 * A lone member is left exactly as it was: one photo is a photo, not a
 * one-member album.
 */
private fun collapseAlbums(
    messages: List<UiMessage>, groupOf: (String) -> String?,
): List<UiMessage> {
    if (messages.isEmpty()) return messages
    val out = ArrayList<UiMessage>(messages.size)
    var i = 0
    while (i < messages.size) {
        val head = messages[i]
        val gid = head.dispatchIdHex?.let(groupOf)
        if (gid == null) {
            out.add(head)
            i++
            continue
        }
        var j = i
        while (j < messages.size && messages[j].dispatchIdHex?.let(groupOf) == gid) j++
        val run = messages.subList(i, j)
        out.add(
            if (run.size == 1) head
            else head.copy(
                content = MessageContent.Album(
                    caption = run.firstNotNullOfOrNull { m -> m.captionOrNull()?.takeIf(String::isNotEmpty) }
                        .orEmpty(),
                    items = run.reversed().map { m ->
                        AlbumItem(m.dispatchIdHex.orEmpty(), m.content)
                    },
                ),
            )
        )
        i = j
    }
    return out
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
    // Not editable — a system row narrates something that already happened.
    is MessageContent.System -> ""
}

/** The caption a media message carries, if it is one. */
private fun UiMessage.captionOrNull(): String? = when (val c = content) {
    is MessageContent.Image -> c.caption
    is MessageContent.Attachment -> c.caption
    else -> null
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
            text = quoted?.takeIf { !it.deleted }?.content,
            outgoing = quoted?.outgoing ?: false,
        )
    }
    val senderHex = senderIpk?.toHex()
    val payload = when {
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

/** kind: 1 = inline Image (blob), else P2P Attachment (thumb + transfer progress). */
private fun MediaRecord.toContent(dispatchIdHex: String, caption: String): MessageContent =
    if (kind.toInt() == 1) MessageContent.Image(
        caption = caption,
        bitmap = blob?.let { decodeAvifCached(dispatchIdHex, it) },
        width = width.toInt(),
        height = height.toInt(),
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
