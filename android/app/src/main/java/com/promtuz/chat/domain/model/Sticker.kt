package com.promtuz.chat.domain.model

import androidx.compose.runtime.Immutable
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.toHex
import uniffi.core.StickerPackRecord
import uniffi.core.StickerRecord

/** Value-based references keep byte-array identity out of Compose state equality. */
@Immutable
data class StickerRef(
    val packHex: String,
    val idHex: String,
    val tokenHex: String,
    val store: Int,
    val width: Int,
    val height: Int,
) {
    /** Cache key for the decoded picture. */
    val key: String get() = "$packHex:$idHex"
}

/** A kept pack with its roster, in picker order. */
@Immutable
data class UiStickerPack(
    val packHex: String,
    val name: String,
    /** Only the creator can append stickers. */
    val mine: Boolean,
    val stickers: List<StickerRef>,
) {
    val cover: StickerRef? get() = stickers.firstOrNull()
}

fun StickerRecord.toRef() = StickerRef(
    packHex = pack.toHex(),
    idHex = id.toHex(),
    tokenHex = token.toHex(),
    store = store.toInt(),
    width = width.toInt(),
    height = height.toInt(),
)

fun StickerRef.toRecord() = StickerRecord(
    pack = packHex.fromHex(),
    id = idHex.fromHex(),
    token = tokenHex.fromHex(),
    store = store.toUShort(),
    width = width.toUInt(),
    height = height.toUInt(),
)

fun StickerPackRecord.toUi() = UiStickerPack(
    packHex = pack.toHex(),
    name = name,
    mine = mine,
    stickers = stickers.map { it.toRef() },
)
