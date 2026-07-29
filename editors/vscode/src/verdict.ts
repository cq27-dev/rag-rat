export function verdictDirectionLabel(direction: string | null): string {
  switch (direction) {
    case 'code_ahead':
      return 'the code moved ahead of this note';
    case 'note_ahead':
      return 'the note moved ahead of the code';
    default:
      return 'direction unknown';
  }
}
