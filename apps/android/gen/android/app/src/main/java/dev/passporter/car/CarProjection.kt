package dev.passporter.car

import android.content.Context
import android.net.Uri
import android.util.Log

/**
 * Whether Android Auto is projecting from this phone onto a car's screen
 * right now: the one condition under which the car actions
 * ([CarActions]) answer, locked phone included (docs/security.md).
 */
fun interface CarProjection {
    /** Whether Android Auto projects now. Blocking; `false` whenever unsure. */
    fun isProjecting(): Boolean
}

/** The [CarProjection] the car path asks: Android Auto's ([AndroidAutoProjection]); tests fake it. */
internal var carProjection: (Context) -> CarProjection = ::AndroidAutoProjection

/**
 * [CarProjection] from Android Auto's own connection provider, the source
 * `androidx.car.app`'s `CarConnection` reads (`content://androidx.car.app.connection`,
 * column `CarConnectionState`, `2` for projection). Read directly, without
 * that library, for two reasons: one synchronous query fits a broadcast
 * receiver better than a `LiveData`, and it checks what the library does
 * not: that the provider belongs to Android Auto ([AUTO_PACKAGE]), so an
 * app that claimed the authority on a phone without Android Auto cannot
 * declare a car. Anything else (no provider, another owner, no row, an
 * error) is no projection: the car path fails closed.
 */
class AndroidAutoProjection(context: Context) : CarProjection {
    private val context = context.applicationContext

    override fun isProjecting(): Boolean {
        val owner = context.packageManager.resolveContentProvider(AUTHORITY, 0)?.packageName
        if (owner != AUTO_PACKAGE) {
            if (owner != null) Log.w(TAG, "the car connection provider is not Android Auto's")
            return false
        }
        return try {
            context.contentResolver.query(URI, arrayOf(COLUMN), null, null, null)?.use { cursor ->
                val column = cursor.getColumnIndex(COLUMN)
                column >= 0 && cursor.moveToFirst() && cursor.getInt(column) == PROJECTION
            } ?: false
        } catch (e: RuntimeException) {
            // A provider that went away or refused (SecurityException) mid-query.
            Log.w(TAG, "could not read the car connection", e)
            false
        }
    }

    internal companion object {
        /** `androidx.car.app.connection.CarConnectionTypeLiveData.CAR_CONNECTION_AUTHORITY`. */
        const val AUTHORITY = "androidx.car.app.connection"

        /** `androidx.car.app.connection.CarConnection.CAR_CONNECTION_STATE`. */
        const val COLUMN = "CarConnectionState"

        /** `androidx.car.app.connection.CarConnection.CONNECTION_TYPE_PROJECTION`. */
        const val PROJECTION = 2

        /** Android Auto's package, which owns [AUTHORITY]. */
        const val AUTO_PACKAGE = "com.google.android.projection.gearhead"

        private const val TAG = "PassporterCar"
        private val URI: Uri = Uri.Builder().scheme("content").authority(AUTHORITY).build()
    }
}
