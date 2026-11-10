const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

// Run the actual dashboard renderers without booting its network/UI listeners.
const html = fs.readFileSync(path.join(__dirname, '../src/index.html'), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
const startup = script.indexOf('\nlet timer;');
assert.ok(startup > 0);
const context = vm.createContext({ URL });
vm.runInContext(script.slice(0, startup), context);
function render(kind, data) {
  context.record = { kind, key: 'synthetic-post', url: 'https://www.instagram.com/p/synthetic/', at: 1700000000, data };
  return vm.runInContext('recordBody(record)', context);
}
const comment = {
  id: 'synthetic-comment', username: 'example_reader', text: 'A synthetic comment.',
  age_label: '2w', likes_display: null, created_at: 1700000000,
  url: 'https://www.instagram.com/p/synthetic/c/synthetic-comment/',
  post_url: 'https://www.instagram.com/p/synthetic/',
};
const post = {
  username: 'example_account', text: 'A synthetic caption.', post_id: 'synthetic-post',
  url: 'https://www.instagram.com/p/synthetic/', created_at: 1700000000,
  stats_display: { likes: '2.3K', comments: '120', shares: null },
  comments_complete: false, comments: [comment],
  capture: { method: 'logged-out Playwright DOM', pagination_attempted: false },
};

test('Instagram posts show captions, displayed counts and explicitly partial comments', () => {
  const out = render('instagram_post', post);
  for (const text of ['example_account', 'A synthetic caption.', '2.3K', '120', 'Shares', 'unknown', 'Captured comments (1)', 'partial', 'example_reader', 'A synthetic comment.', 'pagination not attempted']) {
    assert.ok(out.includes(text), `missing ${text}`);
  }
  assert.ok(!out.includes('2300'), 'rounded labels must not become exact counts');
  assert.match(out, /<details[^>]*open/);
});

test('zero displayed likes stays zero, but absent counts stay unknown', () => {
  const out = render('instagram_post', { ...post, stats_display: { likes: 0 }, comments: [], comments_status: 'no_comments_shown' });
  assert.match(out, />0<\/dd>/);
  assert.equal((out.match(/>unknown<\/dd>/g) || []).length, 2);
  assert.ok(out.includes('No comments yet'));
  assert.ok(out.includes('completeness not verified'));
});

test('comment records include author, text, time, likes and original links', () => {
  const out = render('instagram_comment', comment);
  for (const text of ['example_reader', 'A synthetic comment.', '2023-11', '2w at capture', 'likes: unknown', 'Original comment', 'Parent post']) {
    assert.ok(out.includes(text), `missing ${text}`);
  }
  assert.ok(out.includes(comment.url));
});

test('Instagram text, labels and metadata are escaped and unsafe links are not clickable', () => {
  const hostile = { ...comment, username: '<img src=x onerror=alert(1)>', text: '<script>alert(1)</script>', likes_display: '<svg/onload=alert(1)>', url: 'javascript:alert(1)', post_url: 'data:text/html,bad' };
  const out = render('instagram_post', { ...post, text: hostile.text, stats_display: { likes: hostile.text }, comments: [hostile], url: hostile.url });
  assert.ok(!out.includes('<script>'));
  assert.ok(!out.includes('<img'));
  assert.ok(!out.includes('<svg'));
  assert.ok(!out.includes('href="javascript:'));
  assert.ok(!out.includes('href="data:'));
  assert.ok(out.includes('&lt;script&gt;'));
});

test('legacy records keep their renderer', () => {
  assert.ok(render('page', { title: 'Legacy page', text: 'Visible text', chars: 12 }).includes('Legacy page'));
  assert.ok(render('post', { author: { handle: 'legacy_author' }, text: 'Legacy post', stats: { likes: 3 } }).includes('3 likes'));
});
