package com.promtuz.chat.ui.media

import com.promtuz.chat.R
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.ui.components.MenuAction
import com.promtuz.chat.ui.components.isMediaTile
import com.promtuz.chat.ui.text.dateAndTime
import com.promtuz.chat.utils.media.AvatarPicture
import android.content.Context

/** Oldest first, so swiping right walks back in time; [messages] is newest first. */
fun chatMediaItems(
    context: Context,
    messages: List<UiMessage>,
    conversation: String,
    chatName: String,
    startKey: String,
    onDelete: (dispatchIdHex: String, outgoing: Boolean) -> Unit,
): Pair<List<MediaItem>, Int> {
    val items = ArrayList<MediaItem>()
    for (msg in messages.asReversed()) {
        if (msg.deleted) continue
        val did = msg.dispatchIdHex ?: continue
        val title = if (msg.outgoing) "You" else msg.senderName ?: chatName
        val subtitle = dateAndTime(context, msg.timestampMs)
        val location = com.promtuz.chat.domain.model.MessageLocation(conversation, chatName, did, msg.timestampMs)
        // Each item deletes itself: an album's photos are separate messages.
        fun delete(target: String) =
            listOf(listOf(MenuAction("Delete", R.drawable.oi_trash, destructive = true) { onDelete(target, msg.outgoing) }))
        fun add(content: MessageContent, target: String, caption: String, group: String? = null) {
            val item = when (content) {
                is MessageContent.Image -> MediaItem(
                    key = target, thumb = content.bitmap, width = content.width, height = content.height,
                    mime = content.mime, encoded = content.encoded, byteSize = content.encoded?.size?.toLong(),
                )
                is MessageContent.Attachment -> {
                    val path = content.localPath ?: return
                    if (!content.isMediaTile || content.transferState != 2) return
                    MediaItem(
                        key = target, thumb = content.thumb, width = content.thumb?.width ?: 1, height = content.thumb?.height ?: 1,
                        shareName = content.name, mime = content.mime, filePath = path, byteSize = content.size,
                        videoPath = path.takeIf { content.mime.startsWith("video/") },
                    )
                }
                else -> return
            }
            items += item.copy(
                title = title, subtitle = subtitle, caption = caption, actions = delete(target), group = group,
                message = location.copy(dispatchId = target),
            )
        }
        when (val c = msg.content) {
            is MessageContent.Image -> add(c, did, c.caption)
            is MessageContent.Attachment -> add(c, did, c.caption)
            is MessageContent.Album -> c.items.forEachIndexed { i, item ->
                add(item.content, item.dispatchIdHex, if (i == 0) c.caption else "", did)
            }
            else -> Unit
        }
    }
    return items to items.indexOfFirst { it.key == startKey }.coerceAtLeast(0)
}

fun pictureItem(key: String, image: AvatarPicture, title: String, actions: List<List<MenuAction>> = emptyList()) =
    MediaItem(
        key = key, thumb = image.poster, width = image.poster.width, height = image.poster.height,
        title = title, actions = actions, mime = "image/avif", encoded = image.encoded,
        byteSize = image.encoded.size.toLong(),
    )
