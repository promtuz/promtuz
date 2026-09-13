package com.promtuz.chat.ui.components

import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import android.Manifest
import android.content.pm.PackageManager
import android.widget.Toast
import androidx.compose.ui.platform.LocalContext
import androidx.core.content.ContextCompat
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import com.promtuz.chat.ui.stage.ChatMotion
import kotlin.math.roundToInt
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.STAGED_ATTACHMENT
import com.promtuz.chat.domain.model.STAGED_IMAGE
import com.promtuz.chat.domain.model.acceptsStaged
import com.promtuz.chat.presentation.viewmodel.ChatVM
import com.promtuz.chat.presentation.viewmodel.ComposerAction
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.appearance.chatBarHaze
import com.promtuz.chat.ui.util.freezeOnExit
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeEffect

/** The bar's own geometry; [ComposerMetrics.composerPx] is built from these. */
private val BarMarginH = 10.dp
private val BarMarginV = 8.dp
private val BarPad = 6.dp
private val BarRadius = 26.dp

/** Gap between composer slots. Lives inside each animated node so a slot that
 *  folds away takes its spacing with it instead of dropping it a frame later. */
private val SlotGap = 8.dp

/**
 * Composer: one blurred pill holding the reply/edit reveal, the input (grows to 6
 * lines) and the accent send circle. The pill owns the clip, background and blur —
 * a staged action grows this surface rather than stacking a second one above it.
 */
@Composable
fun ChatBottomBar(
    viewModel: ChatVM, haze: HazeState, metrics: ComposerMetrics,
    onJumpTo: (String) -> Unit = {},
) {
    val input by viewModel.input.collectAsState()
    val action by viewModel.composerAction.collectAsState()
    val busy by viewModel.composerBusy.collectAsState()
    val error by viewModel.composerError.collectAsState()
    val feedbackContext = LocalContext.current
    LaunchedEffect(error) { error?.let { Toast.makeText(feedbackContext, it, Toast.LENGTH_LONG).show() } }

    // The attach panel swaps with the keyboard, so its open-state and the system
    // pickers live here — both the paperclip toggle and the panel's tabs reach them.
    var attachOpen by remember { mutableStateOf(false) }
    // Which close path we're on: field tapped (keyboard coming — hold the panel
    // until it covers) vs back/paperclip (no keyboard — slide down). AttachPanel reads it.
    var closingToKeyboard by remember { mutableStateOf(false) }
    val focusManager = LocalFocusManager.current

    // Permissionless system pickers (photo-picker / SAF), so no storage permission needed.
    val photoPicker = rememberLauncherForActivityResult(ActivityResultContracts.PickMultipleVisualMedia()) { uris ->
        if (uris.isNotEmpty()) { viewModel.attachPhotos(uris); attachOpen = false }
    }
    val filePicker = rememberLauncherForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
        if (uris.isNotEmpty()) { viewModel.attachFiles(uris); attachOpen = false }
    }

    // Back closes the panel before the nav stack.
    BackHandler(attachOpen) { closingToKeyboard = false; attachOpen = false }

    // Voice notes. The mic is asked for on the first tap rather than up front:
    // a chat that never records never sees the prompt.
    val context = LocalContext.current
    val recording by viewModel.recording.collectAsState()
    val beginRecording = {
        if (!viewModel.startRecording()) {
            Toast.makeText(context, "Microphone is busy", Toast.LENGTH_SHORT).show()
        } else { closingToKeyboard = false; attachOpen = false; focusManager.clearFocus() }
    }
    val micPermission = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (granted) beginRecording()
        else Toast.makeText(context, "Voice messages need the microphone", Toast.LENGTH_SHORT).show()
    }
    val onMic = {
        if (ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO) ==
            PackageManager.PERMISSION_GRANTED
        ) beginRecording() else micPermission.launch(Manifest.permission.RECORD_AUDIO)
    }
    BackHandler(action != null && !attachOpen && recording == null) { viewModel.cancelComposerAction() }
    BackHandler(recording != null) { viewModel.cancelRecording() }
    // Leaving the screen ends the note rather than letting it run on: the
    // platform mutes a backgrounded mic, so what would be recorded is silence,
    // and the cap would then send that silence with nobody having tapped send.
    LifecycleEventEffect(Lifecycle.Event.ON_STOP) { viewModel.cancelRecording() }

    // No .imePadding()/.navigationBarsPadding(): AttachPanel owns the bottom region
    // and reserves the keyboard/nav space itself (see its region formula).
    // The pill's own chrome, which the input row's measured height knows nothing about.
    val minimumPill = with(LocalDensity.current) { (38.dp + (BarPad + BarMarginV) * 2).roundToPx() }
    val actionProgress = remember { Animatable(0f) }
    val stripProgress = remember { Animatable(0f) }

    Layout(modifier = Modifier.fillMaxWidth(), content = {
        // Block content is captured (not read live) so the close animation has
        // something to draw after the action nulls.
        var lastAction by remember { mutableStateOf(action) }
        if (action != null) lastAction = action

        LaunchedEffect(action != null) {
            actionProgress.animateTo(if (action != null) 1f else 0f, ChatMotion.spec())
        }

        // Same capture-for-exit as the action block: the strip needs tiles to draw
        // while it closes, after the buffer has already emptied.
        val staged by viewModel.staged.collectAsState()
        var lastStaged by remember { mutableStateOf(staged) }
        if (staged.isNotEmpty()) lastStaged = staged

        LaunchedEffect(staged.isEmpty()) {
            stripProgress.animateTo(if (staged.isEmpty()) 0f else 1f, ChatMotion.spec())
        }

        Box(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = BarMarginH, vertical = BarMarginV)
                .clip(RoundedCornerShape(BarRadius))
                // Freeze must sit on the same chain as the blur it bakes (screen-space
                // Haze shatters under the exiting nav card's scale).
                .freezeOnExit()
                .hazeEffect(haze, chatBarHaze())
                .padding(BarPad),
        ) {
            ComposerSlots(metrics) {
                Reveal(actionProgress) {
                    lastAction?.let {
                        ComposerActionBlock(
                            it,
                            onCancel = { if (!busy && action != null) viewModel.cancelComposerAction() },
                            // Editing can't reach the body's media from the field, so
                            // the block's line opens the picker — narrowed to whatever
                            // the target may legally become.
                            onAddMedia = { if (!busy && action != null) attachOpen = true },
                            onJumpTo = { if (action != null) onJumpTo(it) },
                        )
                    }
                }
                Reveal(stripProgress) {
                    StagedStrip(lastStaged, viewModel::unstage)
                }
                // The recorder takes the input row's place, not a row of its own:
                // the pill keeps one height and the stage under it never moves.
                AnimatedContent(
                    targetState = recording != null,
                    transitionSpec = { fadeIn(ChatMotion.spec()).togetherWith(fadeOut(ChatMotion.spec())) },
                    modifier = Modifier.animateContentSize(ChatMotion.spec(), alignment = Alignment.BottomStart),
                    label = "composerOrRecorder",
                ) { isRecording ->
                    if (isRecording) RecordingRow(
                        viewModel,
                        onCancel = viewModel::cancelRecording,
                        onSend = viewModel::finishRecording,
                    ) else ComposerRow(
                        viewModel, input, action,
                        attachOpen = attachOpen,
                        onToggleAttach = {
                            if (attachOpen) {
                                closingToKeyboard = false // paperclip close → no keyboard, slide down
                                attachOpen = false
                            } else {
                                attachOpen = true // AttachPanel hides the keyboard once it's present (order matters)
                            }
                        },
                        onFieldFocused = {
                            if (attachOpen) { closingToKeyboard = true; attachOpen = false } // keyboard taking over
                        },
                        onMic = onMic,
                    )
                }
            }
        }
        // Editing narrows what may be picked to what the target's body can legally
        // become — the client half of libcore's revision matrix, so a swap the core
        // would refuse is never on offer.
        val editing = (action as? ComposerAction.Edit)?.msg?.content
        AttachPanel(
            open = attachOpen,
            closingToKeyboard = closingToKeyboard,
            haze = haze,
            allowPhotos = editing?.acceptsStaged(STAGED_IMAGE) ?: true,
            allowFiles = editing?.acceptsStaged(STAGED_ATTACHMENT) ?: true,
            onHideKeyboard = { focusManager.clearFocus() },
            onPickPhotos = {
                photoPicker.launch(PickVisualMediaRequest(if (action is ComposerAction.Edit) ActivityResultContracts.PickVisualMedia.ImageOnly else ActivityResultContracts.PickVisualMedia.ImageAndVideo))
            },
            onPickFiles = { filePicker.launch(arrayOf("*/*")) },
            onSendPhotos = { uris ->
                viewModel.attachPhotos(uris)
                closingToKeyboard = false
                attachOpen = false
            },
        )
    }) { children, constraints ->
        // Reserve the keyboard/panel first, but always leave a usable input row.
        // Read back allocated heights, never the panel's requested inset.
        val available = constraints.maxHeight
        val region = children[1].measure(constraints.copy(minHeight = 0,
            maxHeight = (available - minimumPill).coerceAtLeast(0)))
        val pill = children[0].measure(constraints.copy(minHeight = 0,
            maxHeight = (available - region.height).coerceAtLeast(0)))
        metrics.composerPx = pill.height
        metrics.regionPx = region.height
        layout(constraints.maxWidth, pill.height + region.height) {
            pill.placeRelative(0, 0)
            region.placeRelative(0, pill.height)
        }
    }
}

/** Input has priority; accessory slots share only the remaining vertical budget. */
@Composable
private fun ComposerSlots(metrics: ComposerMetrics, content: @Composable () -> Unit) {
    Layout(content = content) { children, constraints ->
        val loose = constraints.copy(minHeight = 0)
        val minimumRow = 38.dp.roundToPx().coerceAtMost(constraints.maxHeight)
        val action = children[0].measure(loose.copy(maxHeight = (constraints.maxHeight - minimumRow).coerceAtLeast(0)))
        val strip = children[1].measure(loose.copy(maxHeight = (constraints.maxHeight - minimumRow - action.height).coerceAtLeast(0)))
        val row = children[2].measure(loose.copy(maxHeight = (constraints.maxHeight - action.height - strip.height).coerceAtLeast(0)))
        metrics.accessoryPx = action.height + strip.height
        layout(constraints.maxWidth, row.height + action.height + strip.height) {
            action.placeRelative(0, 0)
            strip.placeRelative(0, action.height)
            row.placeRelative(0, action.height + strip.height)
        }
    }
}

/**
 * A reveal slot: reports [progress] of its content's height and places the
 * content at its own top edge. The content rides the bar's rising edge and is
 * uncovered top-down — a reply's label before its snippet — with the fade
 * tracking the same value so the clip line never reads as a cut.
 *
 * The parent measures the revealed allocation and publishes that actual height
 * for the message stage, including any constraints on this slot.
 */
@Composable
private fun Reveal(
    progress: Animatable<Float, *>, content: @Composable () -> Unit,
) {
    Layout(
        content = content,
        modifier = Modifier
            .clipToBounds()
            .graphicsLayer { alpha = progress.value },
    ) { measurables, constraints ->
        val p = measurables.firstOrNull()?.measure(constraints)
        if (p == null) layout(0, 0) {}
        else {
            val h = (p.height * progress.value).roundToInt().coerceIn(0, p.height)
            layout(p.width, h) { p.placeRelative(0, 0) }
        }
    }
}

/**
 * The staged reply/edit line — label, one-line snippet, cancel — drawn straight
 * onto the bar's surface. One line always, so a swap never resizes the bar and the
 * snippet can roll in place ([AppBarDynamicTitle]'s treatment); the label, being
 * one of two fixed strings, just cuts.
 */
@Composable
private fun ComposerActionBlock(
    action: ComposerAction, onCancel: () -> Unit, onAddMedia: () -> Unit,
    onJumpTo: (String) -> Unit,
) {
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current
    val content = action.msg.content
    val editing = action is ComposerAction.Edit
    // Only an edit whose target can legally take a staged pick offers the media
    // line; an album can't (a revision targets one message, it is several), so
    // there it stays a plain label rather than a control that opens an empty picker.
    val offersMedia = editing && !action.msg.deleted && content !is MessageContent.Album
    val label = if (editing) "Editing" else "Replying to"

    val thumb = when (content) {
        is MessageContent.Image -> content.bitmap
        is MessageContent.Attachment -> content.thumb
        // An album's cover is its first member — the one that carries the caption.
        is MessageContent.Album -> content.items.firstOrNull()?.content?.let {
            when (it) {
                is MessageContent.Image -> it.bitmap
                is MessageContent.Attachment -> it.thumb
                else -> null
            }
        }
        else -> null
    }

    // A reply names what it's answering. An edit doesn't: the text is already in
    // the field and editable, so repeating it is noise — the line offers the one
    // part of the body the composer can't otherwise reach.
    val snippet = when {
        action.msg.deleted -> "Deleted message"
        editing -> when (content) {
            is MessageContent.Image -> "Tap to replace photo"
            is MessageContent.Attachment -> "Tap to replace file"
            // An album is several messages; a revision targets one, so there's
            // nothing here to offer.
            is MessageContent.Album -> content.caption.ifEmpty { "${content.items.size} photos" }
            else -> "Tap to add media"
        }
        content is MessageContent.Image -> content.caption.ifEmpty { "Photo" }
        content is MessageContent.Attachment ->
            content.caption.ifEmpty { content.name.ifEmpty { "File" } }
        content is MessageContent.Album ->
            content.caption.ifEmpty { "${content.items.size} photos" }
        content is MessageContent.Voice -> "Voice message"
        content is MessageContent.Text -> content.text
        else -> ""
    }

    Row(
        Modifier
            .fillMaxWidth()
            .padding(start = 10.dp, top = 4.dp, bottom = 10.dp, end = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (content !is MessageContent.Text && !action.msg.deleted) {
            Box(
                Modifier
                    .padding(end = 8.dp)
                    .size(36.dp)
                    .clip(RoundedCornerShape(8.dp))
                    .background(colors.surfaceVariant),
                contentAlignment = Alignment.Center,
            ) {
                if (thumb != null) Image(thumb, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop)
                // A file with no preview still needs a mark, or the tile reads as a
                // failed image rather than a document.
                else DrawableIcon(
                    if (content is MessageContent.Voice) R.drawable.i_mic else R.drawable.oi_paperclip,
                    Modifier.size(16.dp),
                    tint = colors.onSurfaceVariant,
                )
            }
        }
        // Editing offers the media the field can't reach; replying names another
        // message, so the block is the way back to it.
        val onLineTap: (() -> Unit)? = when {
            offersMedia -> onAddMedia
            !editing -> action.msg.dispatchIdHex?.let { did -> { onJumpTo(did) } }
            else -> null
        }
        Column(
            Modifier
                .weight(1f)
                .then(onLineTap?.let { Modifier.clickable(onClick = it) } ?: Modifier),
        ) {
            Text(label, style = MaterialTheme.typography.labelMedium, color = chat.accent)
            AnimatedContent(
                targetState = snippet,
                transitionSpec = {
                    (slideInVertically(ChatMotion.spec()) { it } + fadeIn(ChatMotion.spec()))
                        .togetherWith(
                            slideOutVertically(ChatMotion.spec()) { -it } + fadeOut(ChatMotion.spec())
                        )
                },
                label = "actionSnippet",
            ) { s ->
                Text(
                    s,
                    style = MaterialTheme.typography.bodyMedium,
                    // The edit line is an affordance, not a quote — tinted so it
                    // reads as something to press rather than something to read.
                    color = if (offersMedia) chat.accent else colors.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
        // Same circle as the row below, not an IconButton: its 48dp minimum exceeds
        // the label+snippet stack and would set the block's height instead of them.
        Box(
            Modifier.size(38.dp).clip(CircleShape).clickable(onClick = onCancel)
                .semantics { contentDescription = "Cancel action" },
            contentAlignment = Alignment.Center,
        ) {
            DrawableIcon(R.drawable.i_close, Modifier.size(18.dp), tint = colors.onSurfaceVariant)
        }
    }
}

/** The input row itself — unstyled; the pill above it owns shape, blur and inset. */
@Composable
private fun ComposerRow(
    viewModel: ChatVM,
    input: String,
    action: ComposerAction?,
    attachOpen: Boolean,
    onToggleAttach: () -> Unit,
    onFieldFocused: () -> Unit,
    onMic: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current

    // Buffered media is a draft on its own — text is optional once something's
    // staged. Held while anything is still encoding: libcore refuses a
    // half-prepared item, so an enabled button there would fail silently.
    val staged by viewModel.staged.collectAsState()
    val busy by viewModel.composerBusy.collectAsState()
    val editing = action as? ComposerAction.Edit
    val canClearCaption = editing?.msg?.content is MessageContent.Image || editing?.msg?.content is MessageContent.Attachment
    val hasContent = input.isNotBlank() || staged.isNotEmpty()
    val hasDraft = (hasContent || canClearCaption) && staged.all { it.ready } && !busy

    Row(
        modifier.fillMaxWidth(),
        verticalAlignment = Alignment.Bottom,
    ) {
        // Swap the leading affordance without changing the input's width.
        Box(
            Modifier.padding(end = SlotGap).size(38.dp).clip(CircleShape)
                .clickable(enabled = hasContent && !busy) { onToggleAttach() }
                .semantics { contentDescription = if (hasContent) "Attach media" else "Stickers" },
            contentAlignment = Alignment.Center,
        ) {
            AnimatedContent(
                targetState = hasContent,
                transitionSpec = {
                    (fadeIn(ChatMotion.spec()) + scaleIn(ChatMotion.spec(), 0.7f))
                        .togetherWith(fadeOut(ChatMotion.spec()) + scaleOut(ChatMotion.spec(), 0.7f))
                },
                label = "leadingSlot",
            ) { drafting ->
                DrawableIcon(
                    if (drafting) R.drawable.oi_paperclip else R.drawable.oi_sticker,
                    Modifier.size(if (drafting) 20.dp else 22.dp),
                    tint = if (attachOpen) chat.accent else colors.onSurfaceVariant,
                )
            }
        }
        BasicTextField(
            value = input,
            onValueChange = { viewModel.input.value = it },
            enabled = !busy,
            textStyle = MaterialTheme.typography.bodyLarge.copy(color = colors.onSurface),
            cursorBrush = SolidColor(chat.accent),
            maxLines = 6,
            // Tapping the field to type raises the keyboard, so close the panel it replaces.
            modifier = Modifier.weight(1f)
                .onFocusChanged { if (it.isFocused) onFieldFocused() }
                .semantics { contentDescription = "Message input" },
            // Floored at the button size and centred within it, so a single line sits
            // level with the icons rather than riding the row's Bottom alignment. Past
            // one line the box grows and Bottom keeps the buttons at the last line.
            decorationBox = { inner ->
                Box(
                    Modifier.heightIn(min = 38.dp).padding(vertical = 7.dp),
                    contentAlignment = Alignment.CenterStart,
                ) {
                    // Staged media makes the field a caption for it, not a message
                    // of its own — the send commits one thing either way.
                    if (input.isEmpty()) Text(
                        if (staged.isEmpty()) "Message" else "Caption",
                        style = MaterialTheme.typography.bodyLarge,
                        color = colors.onSurfaceVariant,
                    )
                    inner()
                }
            },
        )


        // The paperclip lives here only while the composer is empty; once there's
        // a draft it has already moved to the leading slot, so this one folds away.
        // The gap rides inside the animated node, so it collapses with the icon
        // rather than vanishing in a frame after it.

        // Keep the same text width while the attachment affordance changes sides.
        Box(Modifier.padding(start = SlotGap).size(38.dp)
            .graphicsLayer { alpha = if (hasContent) 0f else 1f }
            .clip(CircleShape).clickable(enabled = !hasContent && !busy) { onToggleAttach() }
            .semantics { contentDescription = "Attach media" },
            contentAlignment = Alignment.Center) {
            DrawableIcon(R.drawable.oi_paperclip, Modifier.size(20.dp),
                tint = if (attachOpen) chat.accent else colors.onSurfaceVariant)
        }

        // The trailing slot is ALWAYS occupied at a fixed size so the pill's
        // height never jumps: mic by default, send when there's a draft,
        // crossfading in place. Solid accent, no haze — a blurred layer under
        // the circle rendered as a square. Held while media is still encoding:
        // the tap then does nothing rather than start a recording under a draft.
        Box(
            Modifier
                .padding(start = SlotGap)
                .size(38.dp)
                .clip(CircleShape)
                .background(if (hasDraft) chat.accent else Color.Transparent)
                // An edit has a body to replace and a voice note is not one,
                // so the mic sits out while one is staged; a reply rides along.
                .clickable(enabled = !busy && (hasDraft || (!hasContent && action !is ComposerAction.Edit))) {
                    if (hasDraft) viewModel.send() else onMic()
                },
            contentAlignment = Alignment.Center,
        ) {
            // Anything drafted shows send, even while it's still encoding and
            // the tap is held: a mic there would promise a recording the slot
            // can't start.
            if (busy) CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp, color = chat.accent)
            else AnimatedContent(
                targetState = when {
                    action is ComposerAction.Edit -> R.drawable.i_edit_check
                    hasContent -> R.drawable.i_send
                    else -> R.drawable.i_mic
                },
                transitionSpec = {
                    (scaleIn(tween(140), 0.6f) + fadeIn(tween(140)))
                        .togetherWith(scaleOut(tween(140), 0.6f) + fadeOut(tween(140)))
                },
                label = "composerAction",
            ) { icon ->
                DrawableIcon(
                    icon,
                    Modifier.size(18.dp),
                    tint = if (hasDraft) colors.onPrimary else colors.onSurfaceVariant,
                )
            }
        }
    }
}

/**
 * The input row while a voice note records: a level-driven red dot, the
 * elapsed time, cancel, and the same accent send circle the draft uses. Tap
 * to start and tap to send rather than hold — one gesture fewer to get wrong,
 * and the note survives a glance away from the screen.
 */
@Composable
private fun RecordingRow(viewModel: ChatVM, onCancel: () -> Unit, onSend: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current
    val recording by viewModel.recording.collectAsState()
    val level by animateFloatAsState(recording?.level ?: 0f, tween(100), label = "micLevel")
    val elapsed = (recording?.elapsedMs ?: 0L) / 1000
    Row(
        Modifier.fillMaxWidth().heightIn(min = 38.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.size(38.dp), contentAlignment = Alignment.Center) {
            Box(
                Modifier
                    .size((10 + 14 * level).dp)
                    .clip(CircleShape)
                    .background(colors.error.copy(alpha = 0.25f + 0.75f * (1f - level))),
            )
        }
        Text(
            "%d:%02d".format(elapsed / 60, elapsed % 60),
            style = MaterialTheme.typography.bodyLarge,
            color = colors.onSurface,
            modifier = Modifier.padding(start = 4.dp),
        )
        Text(
            "Recording…",
            style = MaterialTheme.typography.bodyMedium,
            color = colors.onSurfaceVariant,
            modifier = Modifier.padding(start = 12.dp).weight(1f),
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        Box(
            Modifier.size(38.dp).clip(CircleShape).clickable(onClick = onCancel),
            contentAlignment = Alignment.Center,
        ) {
            DrawableIcon(R.drawable.i_close, Modifier.size(18.dp), tint = colors.onSurfaceVariant)
        }
        Box(
            Modifier
                .padding(start = SlotGap)
                .size(38.dp)
                .clip(CircleShape)
                .background(chat.accent)
                .clickable(onClick = onSend),
            contentAlignment = Alignment.Center,
        ) {
            DrawableIcon(R.drawable.i_send, Modifier.size(18.dp), tint = colors.onPrimary)
        }
    }
}
