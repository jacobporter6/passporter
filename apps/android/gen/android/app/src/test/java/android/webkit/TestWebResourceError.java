package android.webkit;

/**
 * A {@link WebResourceError} for tests: its constructor is package-private,
 * so only a class in this package can extend it.
 */
public final class TestWebResourceError extends WebResourceError {
    @Override
    public int getErrorCode() {
        return WebViewClient.ERROR_HOST_LOOKUP;
    }

    @Override
    public CharSequence getDescription() {
        return "net::ERR_NAME_NOT_RESOLVED";
    }
}
