/**
 * Single source of truth for the two external URLs this site points at.
 *
 * Shade is not published on GitHub yet, so both values are placeholders.
 * Change them here and every link, meta tag and sitemap entry follows.
 */

export const REPO_URL = 'https://github.com/OmarAlex24/shade';

/** Canonical origin used for og:url, the sitemap and the canonical link. */
export const SITE_URL = 'https://omaralex24.github.io/shade';

/** Convenience builder for a file or directory inside the repository. */
export const repoPath = (path: string): string => `${REPO_URL}/blob/main/${path}`;

/** Convenience builder for a directory listing inside the repository. */
export const repoTree = (path: string): string => `${REPO_URL}/tree/main/${path}`;

export const VERSION = '0.1.0';
export const LICENSE = 'MIT';
