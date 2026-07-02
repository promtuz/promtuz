package com.promtuz.chat.domain.model

import com.promtuz.chat.ui.activities.ShareIdentity
import com.promtuz.chat.utils.extensions.structuralEquals
import com.promtuz.chat.utils.extensions.structuralHash
import kotlinx.serialization.Serializable
import kotlin.reflect.KProperty1

// private const val QR_MAGIC_NUMBER: UInt = 0x0750545au

/**
 *  Identity data class is used in exchanging public keys using QR on [ShareIdentity]
 *
 *  Leaving this undeleted for now, but this will not be used anymore.
 *  Anything related to this will be migrated to `libcore` in future
 */
@Serializable
data class Identity(
    val ipk: ByteArray,
    val epk: ByteArray,
    val vfk: ByteArray,
    val addr: String?,
    val nickname: String?
) {
    override fun equals(other: Any?) = structuralEquals(other)
    override fun hashCode() = structuralHash()

    override fun toString(): String {
        fun Any?.fmt(): String = when (this) {
            is ByteArray -> joinToString("") { "%02x".format(it) }
            else -> toString()
        }

        val fields = this::class.members
            .filterIsInstance<KProperty1<Identity, *>>()
            .joinToString("\n") { "  ${it.name}: ${it.get(this).fmt()}" }

        return "${this::class.simpleName} (\n$fields\n)"
    }
}