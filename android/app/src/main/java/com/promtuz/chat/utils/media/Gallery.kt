package com.promtuz.chat.utils.media

import android.content.ContentUris
import android.content.Context
import android.net.Uri
import android.os.Build
import android.provider.MediaStore
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

data class GalleryItem(val uri: Uri, val isVideo: Boolean, val durationMs: Long)

/**
 * Newest-first images+videos from the device MediaStore, for the Photos tab grid.
 * ponytail: flat 500-item cap instead of paged loading — plenty for a picker, revisit
 * if devices with huge libraries make the query itself slow.
 */
suspend fun loadGallery(context: Context, limit: Int = 500): List<GalleryItem> = withContext(Dispatchers.IO) {
    val collection = MediaStore.Files.getContentUri("external")
    // DURATION on the Files collection exists only from API 29; older devices throw
    // on the column, so omit it there and leave video durations unknown.
    val hasDuration = Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q
    val projection = buildList {
        add(MediaStore.Files.FileColumns._ID)
        add(MediaStore.Files.FileColumns.MEDIA_TYPE)
        if (hasDuration) add(MediaStore.Files.FileColumns.DURATION)
    }.toTypedArray()
    val selection = "${MediaStore.Files.FileColumns.MEDIA_TYPE} IN (?, ?)"
    val selectionArgs = arrayOf(
        MediaStore.Files.FileColumns.MEDIA_TYPE_IMAGE.toString(),
        MediaStore.Files.FileColumns.MEDIA_TYPE_VIDEO.toString(),
    )
    // No "LIMIT n" in the sort string — MediaStore rejects it as an invalid token on
    // API 30+ (crash). Cap while reading the cursor instead.
    val sort = "${MediaStore.Files.FileColumns.DATE_ADDED} DESC"

    val items = mutableListOf<GalleryItem>()
    context.contentResolver.query(collection, projection, selection, selectionArgs, sort)?.use { cursor ->
        val idCol = cursor.getColumnIndexOrThrow(MediaStore.Files.FileColumns._ID)
        val typeCol = cursor.getColumnIndexOrThrow(MediaStore.Files.FileColumns.MEDIA_TYPE)
        val durationCol = if (hasDuration) cursor.getColumnIndex(MediaStore.Files.FileColumns.DURATION) else -1
        while (cursor.moveToNext() && items.size < limit) {
            val id = cursor.getLong(idCol)
            val isVideo = cursor.getInt(typeCol) == MediaStore.Files.FileColumns.MEDIA_TYPE_VIDEO
            val duration = if (isVideo && durationCol >= 0) cursor.getLong(durationCol) else 0L
            items += GalleryItem(
                uri = ContentUris.withAppendedId(collection, id),
                isVideo = isVideo,
                durationMs = duration,
            )
        }
    }
    items
}
