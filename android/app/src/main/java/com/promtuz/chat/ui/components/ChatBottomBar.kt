package com.promtuz.chat.ui.components

import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.expandHorizontally
import androidx.compose.animation.shrinkHorizontally
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
import androidx.compose.foundation.layout.Arrangement
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
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
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

    // No .imePadding()/.navigationBarsPadding(): AttachPanel owns the bottom region
    // and reserves the keyboard/nav space itself (see its region formula).
    // The pill's own chrome, which the input row's measured height knows nothing about.
    val chromePx = with(LocalDensity.current) { ((BarPad + BarMarginV) * 2).roundToPx() }

    Column(Modifier.fillMaxWidth()) {
        // Block content is captured (not read live) so the close animation has
        // something to draw after the action nulls.
        var lastAction by remember { mutableStateOf(action) }
        if (action != null) lastAction = action

        LaunchedEffect(action != null) {
            metrics.progress.animateTo(if (action != null) 1f else 0f, ChatMotion.spec())
        }

        // Same capture-for-exit as the action block: the strip needs tiles to draw
        // while it closes, after the buffer has already emptied.
        val staged by viewModel.staged.collectAsState()
        var lastStaged by remember { mutableStateOf(staged) }
        if (staged.isNotEmpty()) lastStaged = staged

        LaunchedEffect(staged.isEmpty()) {
            metrics.stripProgress.animateTo(if (staged.isEmpty()) 0f else 1f, ChatMotion.spec())
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
            Column {
                Reveal(metrics.progress, { metrics.actionH = it }) {
                    lastAction?.let {
                        ComposerActionBlock(
                            it,
                            onCancel = viewModel::cancelComposerAction,
                            // Editing can't reach the body's media from the field, so
                            // the block's line opens the picker — narrowed to whatever
                            // the target may legally become.
                            onAddMedia = { attachOpen = true },
                            onJumpTo = onJumpTo,
                        )
                    }
                }
                Reveal(metrics.stripProgress, { metrics.stripH = it }) {
                    StagedStrip(lastStaged, viewModel::unstage)
                }
                ComposerRow(
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
                    modifier = Modifier.onSizeChanged { metrics.composerPx = it.height + chromePx },
                )
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
            metrics = metrics,
            allowPhotos = editing?.acceptsStaged(STAGED_IMAGE) ?: true,
            allowFiles = editing?.acceptsStaged(STAGED_ATTACHMENT) ?: true,
            onHideKeyboard = { focusManager.clearFocus() },
            onPickPhotos = {
                photoPicker.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageAndVideo))
            },
            onPickFiles = { filePicker.launch(arrayOf("*/*")) },
            onSendPhotos = { uris ->
                viewModel.attachPhotos(uris)
                closingToKeyboard = false
                attachOpen = false
            },
        )
    }
}

/**
 * A reveal slot: reports [progress] of its content's height and places the
 * content at its own top edge. The content rides the bar's rising edge and is
 * uncovered top-down — a reply's label before its snippet — with the fade
 * tracking the same value so the clip line never reads as a cut.
 *
 * Height goes out through [onHeight] rather than being measured by the caller:
 * the stage reads the same metrics this lays out from, so the bar and the
 * messages resolve in one pass instead of a frame apart.
 */
@Composable
private fun Reveal(
    progress: Animatable<Float, *>, onHeight: (Int) -> Unit, content: @Composable () -> Unit,
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
            onHeight(p.height)
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
                    R.drawable.oi_paperclip,
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
            Modifier.size(38.dp).clip(CircleShape).clickable(onClick = onCancel),
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
    modifier: Modifier = Modifier,
) {
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current

    // Buffered media is a draft on its own — text is optional once something's
    // staged. Held while anything is still encoding: libcore refuses a
    // half-prepared item, so an enabled button there would fail silently.
    val staged by viewModel.staged.collectAsState()
    val hasContent = input.isNotBlank() || staged.isNotEmpty()
    val hasDraft = hasContent && staged.all { it.ready }

    Row(
        modifier.fillMaxWidth(),
        verticalAlignment = Alignment.Bottom,
    ) {
        // The leading slot never empties — it SWAPS. A sticker is a whole message
        // of its own, so it can't coexist with a draft; the paperclip can, and
        // moves over from the trailing side to take the place. Keeping the slot
        // occupied is also what keeps the row's geometry still: collapsing it
        // animated the icon away but dropped its spacing in one frame.
        Box(
            Modifier.padding(end = SlotGap).size(38.dp).clip(CircleShape)
                .clickable(enabled = hasContent) { onToggleAttach() },
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
            textStyle = MaterialTheme.typography.bodyLarge.copy(color = colors.onSurface),
            cursorBrush = SolidColor(chat.accent),
            maxLines = 6,
            // Tapping the field to type raises the keyboard, so close the panel it replaces.
            modifier = Modifier.weight(1f)
                .onFocusChanged { if (it.isFocused) onFieldFocused() },
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
        AnimatedVisibility(
            visible = !hasContent,
            enter = fadeIn(ChatMotion.spec()) + expandHorizontally(ChatMotion.spec()),
            exit = fadeOut(ChatMotion.spec()) + shrinkHorizontally(ChatMotion.spec()),
        ) {
            val attachRot by animateFloatAsState(if (attachOpen) 45f else 0f, tween(200), label = "attachRot")
            Box(
                Modifier.padding(start = SlotGap).size(38.dp).clip(CircleShape)
                    .clickable(onClick = onToggleAttach),
                contentAlignment = Alignment.Center,
            ) {
                DrawableIcon(
                    R.drawable.oi_paperclip,
                    Modifier.size(20.dp).rotate(attachRot),
                    tint = if (attachOpen) chat.accent else colors.onSurfaceVariant,
                )
            }
        }

        // The trailing slot is ALWAYS occupied at a fixed size so the pill's
        // height never jumps: mic by default (voice notes soon), send when
        // there's a draft, crossfading in place. Solid accent, no haze — a
        // blurred layer under the circle rendered as a square.
        Box(
            Modifier
                .padding(start = SlotGap)
                .size(38.dp)
                .clip(CircleShape)
                .background(if (hasDraft) chat.accent else Color.Transparent)
                .clickable(enabled = hasDraft) { viewModel.send() },
            contentAlignment = Alignment.Center,
        ) {
            AnimatedContent(
                targetState = when {
                    action is ComposerAction.Edit && hasDraft -> R.drawable.i_edit_check
                    hasDraft -> R.drawable.i_send
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
